#[cfg(debug_assertions)]
use std::{fs::File, io::Write};
#[cfg(debug_assertions)]
use tokio::sync::Mutex;

use {
    crossterm::event::{self, Event, KeyCode, KeyEvent},
    openssl::{
        pkey::Private,
        rsa::{Padding, Rsa},
        symm::{encrypt, Cipher},
    },
    std::{net::SocketAddr, sync::Arc, time::Duration},
    tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::mpsc::{channel, Receiver, Sender},
    },
};

const IP: &str = "0.0.0.0:42530";
const SYMM: usize = 32;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(debug_assertions)]
    let f = Arc::new(Mutex::new(
        std::fs::OpenOptions::new()
            .append(true)
            .write(true)
            .create(true)
            .open("latest.log")?,
    ));
    let listener = TcpListener::bind(IP).await?;
    println!("Listening on {IP}");

    let mut check = tokio::task::spawn_blocking(|| loop {
        if let Ok(Event::Key(KeyEvent {
            code: KeyCode::Esc, ..
        })) = event::read()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    });

    let (tx_master, mut rx_master) = channel::<Vec<u8>>(25);
    let sv_rsa = Rsa::generate(2048)?;
    let rsa_arc = Arc::new(sv_rsa);
    let tx_arc = Arc::new(tx_master);
    let mut senders = vec![];
    let mut tasks = vec![];

    loop {
        tokio::select! {
            Ok((stream, addr)) = listener.accept() => {
                println!("Incoming connection from {addr}");
                let (tx, rx) = channel::<Vec<u8>>(25);
                let tx_clone = Arc::clone(&tx_arc);
                let rsa_clone = Arc::clone(&rsa_arc);

                #[cfg(debug_assertions)]
                let f_clone = Arc::clone(&f);

                tasks.push((tokio::spawn(async move {
                    #[cfg(debug_assertions)]
                    handle_connection(stream, addr, rx, tx_clone, rsa_clone, f_clone).await;
                    #[cfg(not(debug_assertions))]
                    handle_connection(stream, addr, rx, tx_clone, rsa_clone).await;
                }), addr));

                senders.push(tx);
            },
            Some(msg) = rx_master.recv() => {
                let mut rms = vec![];
                for (idx, sender) in senders.iter().enumerate() {
                    let Ok(_) = sender.send(msg.clone()).await else {
                        rms.push(idx);
                        continue;
                    };
                }
                for r in rms {
                    senders.remove(r);
                }
            },
            _ = &mut check => {
                for task in tasks {
                    if !task.0.is_finished() {
                        task.0.abort();
                        println!("Closing connection with {}", task.1);
                    }
                }
                println!("Server shutting down");
                break Ok(());
            },
        }
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    addr: SocketAddr,
    mut rx: Receiver<Vec<u8>>,
    tx: Arc<Sender<Vec<u8>>>,
    sv_rsa: Arc<Rsa<Private>>,
    #[cfg(debug_assertions)] f: Arc<Mutex<File>>,
) {
    let ciph = Cipher::aes_256_cbc();
    let mut first = true;
    loop {
        let mut typ_buf = [0u8; 3];
        let mut len_buf = [0u8; 4];
        tokio::select! {
            result = stream.read_exact(&mut typ_buf) => {
                match result {
                    Ok(0) => break,
                    Ok(_) => {
                        stream.read_exact(&mut len_buf).await.unwrap();
                        match &typ_buf {
                            b"ENC" => {
                                let key_len = u32::from_be_bytes(len_buf) as usize;
                                let mut key = vec![0u8; key_len];
                                stream.read_exact(&mut key).await.unwrap();

                                let mut msg_len_buf = [0u8; 4];
                                stream.read_exact(&mut msg_len_buf).await.unwrap();
                                let msg_len = u32::from_be_bytes(msg_len_buf) as usize;
                                let mut msg = vec![0u8; msg_len];
                                stream.read_exact(&mut msg).await.unwrap();

                                #[cfg(debug_assertions)]
                                {
                                    let mut fl = f.lock().await;
                                    let v = [typ_buf.as_slice(), &len_buf, &key, &msg_len_buf, &msg].concat();
                                    let s = String::from_utf8_lossy(&v);
                                    writeln!(fl, "{s}").unwrap();
                                }

                                tx.send([typ_buf.as_slice(), &len_buf, &key, &msg_len_buf, &msg].concat()).await.unwrap();
                            },
                            b"PUB" if first => {
                                let key_len = u32::from_be_bytes(len_buf) as usize;
                                let mut key_buf = vec![0u8; key_len];
                                if stream.read_exact(&mut key_buf).await.is_err() {
                                    break;
                                }
                                let cl_r = Rsa::public_key_from_der(&key_buf).unwrap();

                                let sv_prv = sv_rsa.private_key_to_der().unwrap();
                                // Encrypt with symm key, encrypt symm key with rsa
                                let symm = gen_rand_symm(SYMM);
                                let enc_symm = {
                                    let mut t = vec![0u8; sv_prv.len()];
                                    let len = cl_r.public_encrypt(&symm, &mut t, Padding::PKCS1).unwrap();
                                    t[0..len].to_owned()
                                };

                                let prv_hed = "PRV".as_bytes();
                                let symm_len = (enc_symm.len() as u32).to_be_bytes();
                                let enc_prv = encrypt(ciph, &symm, None, &sv_prv).unwrap();

                                let prv_len = (enc_prv.len() as u32).to_be_bytes();
                                stream.write_all(&[prv_hed, &symm_len, &enc_symm, &prv_len, &enc_prv].concat()).await.unwrap();

                                first = false;
                            },
                            _ => eprintln!("Error reading from stream: {result:?}"),
                        }
                    },
                    Err(e) if e.kind() == tokio::io::ErrorKind::UnexpectedEof => {
                        break;
                    },
                    Err(e) => {
                        eprintln!("Error reading from client: {e:?}");
                        break;
                    }
                }
            },
            Some(msg) = rx.recv() => {
                #[cfg(debug_assertions)]
                {
                    let mut fl = f.lock().await;
                    let kl = u32::from_be_bytes([msg[3], msg[4], msg[5], msg[6]]) as usize;
                    writeln!(fl, "KL: {kl}").unwrap();
                    let key = &msg[7..7+kl];
                    let mv = &msg[7+kl..7+kl+4];
                    let ml = u32::from_be_bytes([mv[0], mv[1], mv[2], mv[3]]);
                    writeln!(fl, "ML: {ml}").unwrap();

                    let dec_key = {
                        let mut t = [0u8; 2048];
                        let len = sv_rsa.private_decrypt(&key, &mut t, Padding::PKCS1).unwrap();
                        t[0..len].to_owned()
                    };

                    let out = openssl::symm::decrypt(ciph, &dec_key, None, &msg[7+kl+4..]).unwrap();

                    let s = String::from_utf8_lossy(&out);
                    writeln!(fl, "Decrypted message: {s}\n").unwrap();
                }
                stream.write_all(&msg).await.unwrap();
            }
        }
    }

    println!("Connection from {addr} has been closed");
}

fn gen_rand_symm(prec: usize) -> Vec<u8> {
    let mut key = vec![0u8; prec];
    openssl::rand::rand_bytes(&mut key).unwrap();
    key
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn dev_tunnel_test() -> Result<(), Box<dyn std::error::Error>> {
        let ip = super::IP;
        let listener = tokio::net::TcpListener::bind(ip).await?;
        println!("Listening on {ip}");

        while let Ok((mut stream, addr)) = listener.accept().await {
            println!("{addr:?}");
            let mut buf = [0u8; 32];
            while let Ok(1..) = stream.read(&mut buf).await {
                let s = String::from_utf8_lossy(&buf);
                print!("{s}");
                buf = [0u8; 32];
            }
        }

        Ok(())
    }
}
