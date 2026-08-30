//! Shared Tokio UDP receive loop for datagram inputs.

use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use tokio::net::UdpSocket;
use tracing::{error, info, warn};

use super::rate_limit::RateLimiter;
use crate::event::Event;
use crate::metrics::InputMetrics;

pub(super) type DatagramValidator = fn(&[u8]) -> std::result::Result<(), String>;

pub(super) async fn shutdown_change_is_terminal(
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
) -> bool {
    match shutdown.changed().await {
        Ok(()) => *shutdown.borrow(),
        Err(_) => true,
    }
}

pub(super) struct UdpInputRuntime {
    pub module_name: &'static str,
    pub bind_addr: String,
    pub rate_limit: Option<u64>,
    pub metrics: Arc<InputMetrics>,
    pub validator: Option<DatagramValidator>,
}

impl UdpInputRuntime {
    pub async fn run(
        self,
        tx: tokio::sync::mpsc::Sender<Event>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let socket = UdpSocket::bind(&self.bind_addr).await?;
        crate::modules::input_startup_ready();
        info!("{} listening on {}", self.module_name, self.bind_addr);

        let limiter = self.rate_limit.map(RateLimiter::new);
        if let Some(rate) = self.rate_limit {
            info!("{} rate_limit: {} events/sec", self.module_name, rate);
        }

        let mut buf = vec![0u8; 65536];
        loop {
            tokio::select! {
                biased;

                terminal = shutdown_change_is_terminal(&mut shutdown) => {
                    if terminal {
                        info!("{}: shutting down", self.module_name);
                        break;
                    }
                }

                result = socket.recv_from(&mut buf) => {
                    match result {
                        Ok((len, addr)) => {
                            let data = &buf[..len];
                            self.metrics.bytes_received.inc_by(len as u64);

                            if let Some(validator) = self.validator
                                && let Err(error) = validator(data)
                            {
                                warn!(
                                    "{} [{}]: dropping invalid message ({})",
                                    self.module_name, addr, error
                                );
                                self.metrics.events_invalid.inc();
                                continue;
                            }

                            self.metrics.events_received.inc();
                            let event = Event::new(Bytes::copy_from_slice(data), addr);

                            if let Some(ref limiter) = limiter {
                                let acquire = limiter.acquire();
                                tokio::pin!(acquire);
                                loop {
                                    tokio::select! {
                                        biased;

                                        terminal = shutdown_change_is_terminal(&mut shutdown) => {
                                            if terminal {
                                                info!("{}: shutting down", self.module_name);
                                                return Ok(());
                                            }
                                        }

                                        () = &mut acquire => break,
                                    }
                                }
                            }

                            let send = tx.send(event);
                            tokio::pin!(send);
                            let send_result = loop {
                                tokio::select! {
                                    biased;

                                    terminal = shutdown_change_is_terminal(&mut shutdown) => {
                                        if terminal {
                                            info!("{}: shutting down", self.module_name);
                                            return Ok(());
                                        }
                                    }

                                    result = &mut send => break result,
                                }
                            };

                            if send_result.is_err() {
                                info!(
                                    "{} [{}]: pipeline event channel closed, stopping input task",
                                    self.module_name, addr
                                );
                                break;
                            }
                        }
                        Err(error) => {
                            error!("{} recv error: {}", self.module_name, error);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket as StdUdpSocket;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    fn pick_port() -> u16 {
        let socket = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        socket.local_addr().unwrap().port()
    }

    fn spawn_runtime(
        bind_addr: String,
        rate_limit: Option<u64>,
        channel_capacity: usize,
    ) -> (
        tokio::task::JoinHandle<Result<()>>,
        tokio::sync::watch::Sender<bool>,
        tokio::sync::mpsc::Receiver<Event>,
        Arc<InputMetrics>,
    ) {
        let metrics = InputMetrics::for_testing();
        let runtime = UdpInputRuntime {
            module_name: "udp_test",
            bind_addr,
            rate_limit,
            metrics: Arc::clone(&metrics),
            validator: None,
        };
        let (tx, rx) = tokio::sync::mpsc::channel(channel_capacity);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(runtime.run(tx, shutdown_rx));
        (handle, shutdown_tx, rx, metrics)
    }

    async fn wait_for_received(metrics: &InputMetrics, expected: u64) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while metrics.events_received.load(Ordering::Relaxed) < expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("UDP runtime did not receive the expected datagrams");
    }

    #[tokio::test]
    async fn shutdown_cancels_rate_limit_token_wait() {
        let bind = format!("127.0.0.1:{}", pick_port());
        let (handle, shutdown, mut events, metrics) = spawn_runtime(bind.clone(), Some(1), 2);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let sender = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        sender.send_to(b"first", &bind).unwrap();
        tokio::time::timeout(Duration::from_millis(500), events.recv())
            .await
            .expect("first event timed out")
            .expect("event channel closed");

        sender.send_to(b"second", &bind).unwrap();
        wait_for_received(&metrics, 2).await;
        shutdown.send(true).unwrap();

        tokio::time::timeout(Duration::from_millis(300), handle)
            .await
            .expect("UDP runtime did not stop while waiting for a rate-limit token")
            .expect("UDP runtime task did not join")
            .expect("UDP runtime returned an error");
        assert!(
            events.recv().await.is_none(),
            "limited event was delivered after shutdown"
        );
    }

    #[tokio::test]
    async fn shutdown_cancels_full_pipeline_channel_send() {
        let bind = format!("127.0.0.1:{}", pick_port());
        let (handle, shutdown, mut events, metrics) = spawn_runtime(bind.clone(), None, 1);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let sender = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        sender.send_to(b"first", &bind).unwrap();
        wait_for_received(&metrics, 1).await;
        tokio::time::timeout(Duration::from_millis(500), async {
            while events.len() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first event did not fill the pipeline channel");

        sender.send_to(b"second", &bind).unwrap();
        wait_for_received(&metrics, 2).await;
        shutdown.send(true).unwrap();

        tokio::time::timeout(Duration::from_millis(300), handle)
            .await
            .expect("UDP runtime did not stop while blocked on the pipeline channel")
            .expect("UDP runtime task did not join")
            .expect("UDP runtime returned an error");
        assert_eq!(
            &events.recv().await.expect("first event missing").ingress[..],
            b"first"
        );
        assert!(
            events.recv().await.is_none(),
            "blocked second event was delivered after shutdown"
        );
    }
}
