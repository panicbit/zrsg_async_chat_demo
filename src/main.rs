extern crate hyper;
extern crate futures;
extern crate tokio_core;
extern crate tokio_io;
extern crate futures_cpupool;

use std::collections::HashMap;
use std::rc::Rc;
use std::cell::RefCell;
use std::iter;
use std::env;
use std::io::{Error, ErrorKind, BufReader};

use futures::Future;
use futures::stream::{self, Stream};
use tokio_core::net::TcpListener;
use tokio_core::reactor::Core;
use tokio_io::io;
use tokio_io::AsyncRead;

use futures::stream::ForEach;
use futures::sync::mpsc::UnboundedSender;
use std::net::SocketAddr;
use futures_cpupool::CpuPool;

fn main() {
    let addr = env::args().nth(1).unwrap_or("127.0.0.1:8080".to_string());
    let addr = addr.parse().unwrap();

    // Create the event loop and TCP listener we'll accept connections on.
    let mut core = Core::new().unwrap();
    let handle = core.handle();
    let socket = TcpListener::bind(&addr, &handle).unwrap();
    println!("Listening on: {}", addr);

    let hyper = Rc::new(hyper::Client::new(&core.handle()));
    let pool = CpuPool::new(1);

    // This is a single-threaded server, so we can just use Rc and RefCell to
    // store the map of all connections we know about.
    let connections = Rc::new(RefCell::new(HashMap::<SocketAddr,UnboundedSender<_>>::new()));

    let srv: ForEach<_,_,_> = socket.incoming().for_each(move |(stream, addr)| {
        println!("New Connection: {}", addr);
        let (reader, writer) = stream.split();

        // Create a channel for our stream, which other sockets will use to
        // send us messages. Then register our address with the stream to send
        // data to us.
        let (tx, rx) = futures::sync::mpsc::unbounded();
        connections.borrow_mut().insert(addr, tx);

        // Define here what we do for the actual I/O. That is, read a bunch of
        // lines from the socket and dispatch them while we also write any lines
        // from other sockets.
        let connections_inner = connections.clone();
        let hyper = hyper.clone();
        let pool = pool.clone();
        let reader = BufReader::new(reader);

        // Model the read portion of this socket by mapping an infinite
        // iterator to each line off the socket. This "loop" is then
        // terminated with an error once we hit EOF on the socket.
        let iter = stream::iter(iter::repeat(()).map(Ok::<(), Error>));
        let socket_reader = iter.fold(reader, move |reader, _| {
            // Read a line off the socket, failing if we're at EOF
            let line = io::read_until(reader, b'\n', Vec::new());
            let line = line.and_then(|(reader, vec)| {
                if vec.len() == 0 {
                    let err = Err(Error::new(ErrorKind::BrokenPipe, "broken pipe"));
                    println!("{:?}", err);
                    err
                } else {
                    Ok((reader, vec))
                }
            });

            // Convert the bytes we read into a string, and then send that
            // string to all other connected clients.
            let line = line.map(|(reader, vec)| {
                (reader, String::from_utf8(vec))
            });
            let connections = connections_inner.clone();
            line.map(move |(reader, message)| {
                println!("{}: {:?}", addr, message);
                let mut conns = connections.borrow_mut();
                if let Ok(msg) = message {
                    // For each open connection except the sender, send the
                    // string via the channel.
                    let iter = conns.iter_mut()
                                    .filter(|&(&k, _)| k != addr)
                                    .map(|(_, v)| v);
                    for tx in iter {
                        tx.send(format!("{}: {}", addr, msg)).unwrap();
                    }
                } else {
                    let tx = conns.get_mut(&addr).unwrap();
                    tx.send("You didn't send valid UTF-8.".to_string()).unwrap();
                }
                reader
            })
        });

        // Whenever we receive a string on the Receiver, we write it to
        // `WriteHalf<TcpStream>`.
        let socket_writer = rx.fold(writer, move |writer, msg| {
            let pool = pool.clone();

            hyper.get("http://www.spiegel.de/".parse().unwrap())
                .and_then(|resp|
                    resp.body().fold(0, |sum, chunk|
                        Ok::<usize,hyper::Error>(sum + chunk.len())
                    )
                )
                .map_err(|_| ())
                .and_then(move |len| pool.spawn_fn(move || {
                    ::std::thread::sleep_ms(10_000);
                    Ok(len)
                }))
                .map(move |len| format!("{}; {}", len, msg))
                .and_then(|msg| io::write_all(writer, msg.into_bytes()).map_err(|_| ()))
                .map_err(|_| ())
                .map(|(writer, _)| writer)
        });

        // Now that we've got futures representing each half of the socket, we
        // use the `select` combinator to wait for either half to be done to
        // tear down the other. Then we spawn off the result.
        let connections = connections.clone();
        let socket_reader = socket_reader.map_err(|_| ());
        let connection = socket_reader.map(|_| ()).select(socket_writer.map(|_| ()));
        handle.spawn(connection.then(move |_| {
            connections.borrow_mut().remove(&addr);
            println!("Connection {} closed.", addr);
            Ok(())
        }));

        Ok(())
    });

    // execute server
    core.run(srv).unwrap();
}
