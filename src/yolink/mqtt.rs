use anyhow::Result;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use uuid::Uuid;

use super::types::YolinkReport;
use crate::auth::TokenManager;

pub struct YolinkMqtt {
    host: String,
    port: u16,
    tokens: Arc<TokenManager>,
}

impl YolinkMqtt {
    pub fn new(host: String, port: u16, tokens: Arc<TokenManager>) -> Self {
        Self { host, port, tokens }
    }

    /// Drive the MQTT event loop forever, reconnecting on error.
    /// Parsed device reports are sent on `tx`.
    ///
    /// `topic_prefix` is the mode-specific namespace:
    /// - Cloud: `yl-home/{home_id}`
    /// - Local: `ylsubnet/{net_id}`
    pub async fn run(self, topic_prefix: String, tx: mpsc::Sender<YolinkReport>) {
        loop {
            match self.run_once(&topic_prefix, &tx).await {
                Ok(()) => {
                    // tx closed — shutting down
                    return;
                }
                Err(e) => {
                    error!(error = %e, "YoLink MQTT connection lost; reconnecting in 10 s");
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        }
    }

    async fn run_once(
        &self,
        topic_prefix: &str,
        tx: &mpsc::Sender<YolinkReport>,
    ) -> Result<()> {
        let token = self.tokens.get_token().await?;
        // Access token as MQTT username; empty password (local hub also accepts this)
        let client_id = format!("hc-yolink-{}", &Uuid::new_v4().to_string()[..8]);

        let mut opts = MqttOptions::new(&client_id, &self.host, self.port);
        opts.set_keep_alive(Duration::from_secs(30));
        opts.set_clean_session(true);
        opts.set_credentials(&token, "");

        let (client, mut eventloop) = AsyncClient::new(opts, 128);

        // Cloud:  yl-home/{home_id}/+/report
        // Local:  ylsubnet/{net_id}/+/report
        let topic = format!("{topic_prefix}/+/report");
        client
            .subscribe(&topic, QoS::AtLeastOnce)
            .await?;

        info!(
            host = %self.host,
            port = self.port,
            %topic,
            "YoLink MQTT subscribed"
        );

        loop {
            match eventloop.poll().await? {
                Event::Incoming(Packet::ConnAck(_)) => {
                    info!("YoLink MQTT ConnAck received");
                }
                Event::Incoming(Packet::Publish(p)) => {
                    // Cloud: yl-home/{home_id}/{device_id}/report  → parts[2] = device_id
                    // Local: ylsubnet/{net_id}/{device_id}/report   → parts[2] = device_id
                    let parts: Vec<&str> = p.topic.split('/').collect();
                    if parts.len() < 4 || parts[3] != "report" {
                        continue;
                    }
                    let device_id = parts[2].to_string();

                    match serde_json::from_slice::<Value>(&p.payload) {
                        Ok(payload) => {
                            let event = payload["event"]
                                .as_str()
                                .unwrap_or("Report")
                                .to_string();
                            let data = payload["data"].clone();
                            let report = YolinkReport { device_id, event, data };
                            if tx.send(report).await.is_err() {
                                // Receiver dropped — plugin is shutting down
                                return Ok(());
                            }
                        }
                        Err(e) => {
                            warn!(topic = %p.topic, error = %e, "Non-JSON YoLink payload, skipping");
                        }
                    }
                }
                Event::Incoming(Packet::Disconnect) => {
                    anyhow::bail!("YoLink MQTT broker sent Disconnect");
                }
                _ => {}
            }
        }
    }
}
