use std::collections::{HashMap, HashSet};
use std::io::{self, ErrorKind};
use std::{cell::RefCell, net::SocketAddr, usize};

use crypto::{digest::Digest, sha2::Sha256};
use mio::{net::TcpListener, unix::UnixReady, Events, Poll, PollOpt, Ready, Token};
use pump::Pump;
use slab::Slab;

const MAX_PUMPS: usize = 2048;
const ROOT_TOKEN: Token = Token(<usize>::max_value() - 1);

pub struct Server {
  sock: TcpListener,
  poll: Poll,
  secret: Vec<u8>,
  pumps: Slab<RefCell<Pump>>,
  zombie: HashSet<Token>,
  links: HashMap<Token, Token>,
}

impl Server {
  pub fn new(addr: SocketAddr, seed: &str) -> Server {
    let mut sha = Sha256::new();
    let mut secret = vec![0u8; sha.output_bytes()];

    sha.input_str(seed);
    sha.result(&mut secret);
    secret.truncate(16);

    Server {
      secret,
      zombie: HashSet::new(),
      sock: TcpListener::bind(&addr).expect("Failed to bind"),
      poll: Poll::new().expect("Failed to create Poll"),
      pumps: Slab::with_capacity(MAX_PUMPS),
      links: HashMap::new(),
    }
  }

  pub fn secret(&self) -> String {
    let secret: Vec<String> = self.secret.iter().map(|b| format!("{:02x}", b)).collect();
    secret.join("")
  }

  pub fn run(&mut self) -> io::Result<()> {
    info!("Starting proxy");
    self
      .poll
      .register(&self.sock, ROOT_TOKEN, Ready::readable(), PollOpt::edge())?;

    let mut events = Events::with_capacity(1024);

    loop {
      self.poll.poll(&mut events, None)?;
      self.dispatch(&events)?;
    }
  }

  fn accept(&mut self) -> io::Result<()> {
    if self.pumps.len() > MAX_PUMPS {
      warn!("max connection limit({}) exceeded", MAX_PUMPS / 2);
      return Ok(());
    }

    let sock = match self.sock.accept() {
      Ok((sock, _)) => sock,
      Err(err) => {
        warn!("accept failed: {}", err);
        return Ok(());
      }
    };

    let pump = Pump::new(sock, &self.secret);
    let idx = self.pumps.insert(RefCell::new(pump));
    let pump = self.pumps.get(idx).unwrap().borrow();

    let token = Token(idx);

    self.poll.register(
      pump.sock(),
      token,
      pump.interest(),
      PollOpt::edge() | PollOpt::oneshot(),
    )?;

    info!(
      "new connection: {:?} from {}",
      token,
      pump.sock().peer_addr()?
    );

    Ok(())
  }

  fn dispatch(&mut self, events: &Events) -> io::Result<()> {
    let mut stale = HashSet::new();
    let mut seen = HashSet::new();
    let mut new_peers = HashMap::new();

    for event in events {
      trace!("{:?}", event);

      let token = event.token();

      if token == ROOT_TOKEN {
        self.accept()?;
        continue;
      }
      seen.insert(token);

      let readiness = UnixReady::from(event.readiness());

      let mut pump = {
        let pump = &self.pumps.get(token.0);
        if pump.is_none() {
          warn!("slab inconsistency");
          continue;
        }
        pump.unwrap().borrow_mut()
      };

      if readiness.is_readable() {
        loop {
          match pump.drain() {
            Ok(peer) => match peer {
              Some(peer_pump) => {
                new_peers.insert(token, peer_pump);
              }
              _ => {}
            },
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
              break;
            }
            Err(e) => {
              warn!("drain failed: {:?}: {}", token, e);
              stale.insert(token);
              break;
            }
          }
        }

        if let Some(peer_token) = self.links.get(&token) {
          self.fan_out(&mut pump, peer_token)?;
        }
      }

      if readiness.is_writable() {
        if let Some(peer_token) = self.links.get(&token) {
          self.fan_in(&mut pump, peer_token)?;
        }

        loop {
          match pump.flush() {
            Ok(_) => {}
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
              break;
            }
            Err(e) => {
              warn!("flush failed: {:?}: {}", token, e);
              stale.insert(token);
              break;
            }
          }
        }
      }

      if readiness.is_hup() || readiness.is_error() {
        stale.insert(token);
      } else {
        self.poll.reregister(
          pump.sock(),
          token,
          pump.interest(),
          PollOpt::edge() | PollOpt::oneshot(),
        )?;
      }
    }

    for (owner, pump) in new_peers {
      let idx = self.pumps.insert(RefCell::new(pump));
      let pump = self.pumps.get(idx).unwrap().borrow();

      let token = Token(idx);

      self.links.insert(token, owner);
      self.links.insert(owner, token);

      self.poll.register(
        pump.sock(),
        token,
        pump.interest(),
        PollOpt::edge() | PollOpt::oneshot(),
      )?;
    }

    self.drop_zombies()?;

    for token in stale {
      self.drop_pump(token)?;
    }

    Ok(())
  }

  fn drop_zombies(&mut self) -> io::Result<()> {
    if self.zombie.len() == 0 {
      return Ok(());
    }

    let zombie: Vec<Token> = self.zombie.iter().cloned().collect();
    self.zombie.clear();

    for token in zombie {
      self.drop_pump(token)?;
    }

    Ok(())
  }

  fn drop_pump(&mut self, token: Token) -> io::Result<()> {
    let pump = self.pumps.remove(token.0);
    let pump = pump.borrow_mut();

    info!("dropping pump: {:?}", token);
    self.poll.deregister(pump.sock())?;

    match self.links.remove(&token) {
      Some(peer_token) => {
        info!("dropping link to peer: {:?}", peer_token);
        self.links.remove(&peer_token);
        self.zombie.insert(peer_token);
      }
      _ => {}
    }
    Ok(())
  }

  fn fan_out(&self, pump: &mut Pump, peer_token: &Token) -> io::Result<bool> {
    trace!("fan out to {:?}", peer_token);
    let buf = pump.pull();
    if buf.is_empty() {
      return Ok(false);
    }

    let peer = self.pumps.get(peer_token.0).unwrap();
    let mut peer = peer.borrow_mut();
    peer.push(&buf);

    self.poll.reregister(
      peer.sock(),
      *peer_token,
      peer.interest(),
      PollOpt::edge() | PollOpt::oneshot(),
    )?;

    Ok(true)
  }

  fn fan_in(&self, pump: &mut Pump, peer_token: &Token) -> io::Result<bool> {
    trace!("fan in from {:?}", peer_token);

    let peer = self.pumps.get(peer_token.0).unwrap();

    let mut peer = peer.borrow_mut();
    let buf = peer.pull();
    if buf.is_empty() {
      return Ok(false);
    }

    pump.push(&buf);

    self.poll.reregister(
      peer.sock(),
      *peer_token,
      peer.interest(),
      PollOpt::edge() | PollOpt::oneshot(),
    )?;

    Ok(true)
  }
}
