//! Background reachability checks for the host list: a timed TCP connect to each host's RDP
//! port every so often. Nothing is sent over the connection.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::mpsc;
use winit::event_loop::EventLoopProxy;

use crate::app::UserEvent;
use crate::ui::widgets::Signal;

const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const PROBE_INTERVAL: Duration = Duration::from_secs(30);
const MAX_CONCURRENT: usize = 16;

pub type Endpoint = (String, u16);

pub struct Prober {
    tx: mpsc::UnboundedSender<Vec<Endpoint>>,
    results: Arc<Mutex<HashMap<Endpoint, Signal>>>,
}

impl Prober {
    pub fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let results = Arc::new(Mutex::new(HashMap::new()));
        let thread_results = Arc::clone(&results);
        let spawned = std::thread::Builder::new()
            .name("probe".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt.block_on(run(rx, thread_results, proxy)),
                    Err(e) => tracing::warn!(error = %e, "host probing disabled"),
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "host probing disabled");
        }
        Self { tx, results }
    }

    /// Replaces the set of hosts to watch and probes them right away.
    pub fn watch(&self, endpoints: Vec<Endpoint>) {
        let _ = self.tx.send(endpoints);
    }

    pub fn signal(&self, host: &str, port: u16) -> Signal {
        self.results
            .lock()
            .get(&(host.to_owned(), port))
            .copied()
            .unwrap_or(Signal::Unknown)
    }
}

async fn run(
    mut rx: mpsc::UnboundedReceiver<Vec<Endpoint>>,
    results: Arc<Mutex<HashMap<Endpoint, Signal>>>,
    proxy: EventLoopProxy<UserEvent>,
) {
    let mut endpoints: Vec<Endpoint> = Vec::new();
    let mut tick = tokio::time::interval(PROBE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            update = rx.recv() => match update {
                Some(list) => {
                    endpoints = list;
                    results.lock().retain(|k, _| endpoints.contains(k));
                }
                None => return,
            },
            _ = tick.tick() => {}
        }

        let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT));
        let mut set = tokio::task::JoinSet::new();
        for endpoint in endpoints.clone() {
            let semaphore = Arc::clone(&semaphore);
            set.spawn(async move {
                let _permit = semaphore.acquire_owned().await;
                let signal = probe(&endpoint.0, endpoint.1).await;
                (endpoint, signal)
            });
        }
        while let Some(Ok((endpoint, signal))) = set.join_next().await {
            results.lock().insert(endpoint, signal);
        }
        let _ = proxy.send_event(UserEvent::Wake);
    }
}

async fn probe(host: &str, port: u16) -> Signal {
    let started = Instant::now();
    match tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect((host, port))).await {
        Ok(Ok(_stream)) => Signal::Rtt(started.elapsed()),
        _ => Signal::Offline,
    }
}
