extern crate alloc;
use crate::Error;
use crate::*;

use crate::node::*;

use tokio::net::UdpSocket;
use tokio::sync::Mutex as TokioMutex;
use tokio::time::{sleep, Duration};

use tracing::*;

use std::net::SocketAddr;
use std::result::Result;
use std::sync::Arc;

use alloc::vec::Vec;
use postcard::*;
use std::marker::PhantomData;

// Quic
use quinn::Endpoint;

use crate::msg::*;
use crate::node::quic::generate_client_config_from_certs;
use chrono::Utc;

impl<T: Message> From<Node<Quic, Idle, T>> for Node<Quic, Active, T> {
    fn from(node: Node<Quic, Idle, T>) -> Self {
        Self {
            __state: PhantomData,
            __data_type: PhantomData,
            cfg: node.cfg,
            runtime: node.runtime,
            stream: node.stream,
            name: node.name,
            topic: node.topic,
            socket: node.socket,
            endpoint: node.endpoint,
            connection: node.connection,
            subscription_data: node.subscription_data,
            task_subscribe: None,
        }
    }
}

impl<T: Message> From<Node<Quic, Idle, T>> for Node<Quic, Subscription, T> {
    fn from(node: Node<Quic, Idle, T>) -> Self {
        Self {
            __state: PhantomData,
            __data_type: PhantomData,
            cfg: node.cfg,
            runtime: node.runtime,
            stream: node.stream,
            name: node.name,
            topic: node.topic,
            socket: node.socket,
            endpoint: node.endpoint,
            connection: node.connection,
            subscription_data: node.subscription_data,
            task_subscribe: node.task_subscribe,
        }
    }
}

impl<T: Message + 'static> Node<Quic, Idle, T> {
    /// Attempt connection from the Node to the Host located at the specified address
    //#[tracing::instrument(skip_all)]
    pub fn activate(mut self) -> Result<Node<Quic, Active, T>, Error> {
        debug!("Attempting QUIC connection");
        self.create_connection();

        Ok(Node::<Quic, Active, T>::from(self))
    }

    fn create_connection(&mut self) {
        let host_addr = self.cfg.network_cfg.host_addr;
        if let Ok((endpoint, connection)) = self.runtime.block_on(async move {
            // QUIC, needs to be done inside of a tokio context
            let client_cfg = generate_client_config_from_certs();
            let client_addr = "0.0.0.0:0".parse::<SocketAddr>().unwrap();
            let mut endpoint = Endpoint::client(client_addr).unwrap();
            endpoint.set_default_client_config(client_cfg);
            match endpoint.connect(host_addr, "localhost") {
                Ok(connecting) => match connecting.await {
                    Ok(connection) => Ok((endpoint, connection)),
                    Err(_) => Err(Error::QuicIssue),
                },
                Err(_) => Err(Error::QuicIssue),
            }
        }) {
            debug!("{:?}", &endpoint.local_addr());
            self.endpoint = Some(endpoint);
            self.connection = Some(connection);
        };
    }

    #[tracing::instrument(skip_all)]
    pub fn subscribe(mut self, rate: Duration) -> Result<Node<Quic, Subscription, T>, Error> {
        let name = self.name.clone();
        let addr = self.cfg.network_cfg.host_addr;
        let topic = self.topic.clone();

        let subscription_data: Arc<TokioMutex<Option<SubscriptionData<T>>>> =
            Arc::new(TokioMutex::new(None));
        let data = Arc::clone(&subscription_data);

        let max_buffer_size = self.cfg.network_cfg.max_buffer_size;
        self.create_connection();
        let connection = self.connection.clone();

        let task_subscribe = self.runtime.spawn(async move {
            let packet = GenericMsg {
                msg_type: MsgType::GET,
                timestamp: Utc::now(),
                name: name.to_string(),
                topic: topic.to_string(),
                data_type: std::any::type_name::<T>().to_string(),
                data: Vec::new(),
            };

            let mut buf = vec![0u8; max_buffer_size];
            loop {
                let packet_as_bytes: Vec<u8> = match to_allocvec(&packet) {
                    Ok(packet) => packet,
                    _ => continue,
                };

                if let Some(connection) = connection.clone() {
                    let _reply = match connection.open_bi().await {
                        Ok((mut send, mut recv)) => {
                            send.write_all(&packet_as_bytes).await.unwrap();
                            send.finish().await.unwrap();

                            match recv.read(&mut buf).await {
                                //Ok(0) => Err(Error::QuicIssue),
                                Ok(Some(n)) => {
                                    let bytes = &buf[..n];

                                    match from_bytes::<GenericMsg>(bytes) {
                                        Ok(reply) => {
                                            {
                                                let timestamp = match data.lock().await.as_ref() {
                                                    Some(data) => {
                                                        debug!("Timestamp: {}", data.timestamp);
                                                        let delta =
                                                            data.timestamp - reply.timestamp;
                                                        // println!("The time difference between msg tx/rx is: {} us",delta);
                                                        if delta <= chrono::Duration::zero() {
                                                            // println!("Data is not newer, skipping to next subscription iteration");
                                                            continue;
                                                        }
                                                    }
                                                    _ => (),
                                                };
                                            }

                                            match from_bytes::<T>(&reply.data) {
                                                Ok(val) => {
                                                    let reply_sub_data = SubscriptionData {
                                                        data: val,
                                                        timestamp: reply.timestamp,
                                                    };
                                                    debug!("QUIC Subscriber received new data");
                                                    let mut data = data.lock().await;

                                                    *data = Some(reply_sub_data);
                                                    sleep(rate).await;
                                                }
                                                _ => continue,
                                            };
                                        }
                                        Err(_) => continue,
                                    }
                                }
                                _ => {
                                    // if e.kind() == std::io::ErrorKind::WouldBlock {}
                                    continue;
                                }
                            }
                        }
                        _ => continue,
                    };
                }
            }
        });
        self.task_subscribe = Some(task_subscribe);

        let mut subscription_node = Node::<Quic, Subscription, T>::from(self);
        subscription_node.subscription_data = subscription_data;

        Ok(subscription_node)
    }
}
