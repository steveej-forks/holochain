#![allow(missing_docs)] // TODO: remove this

//! This module implements value sharing for out-of-band communication between test agents.

use anyhow::{bail, Result as Fallible};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{collections::BTreeMap, sync::Arc};

const TEST_SHARED_VALUES_TYPE: &str = "TEST_SHARED_VALUES_TYPE";
const TEST_SHARED_VALUES_TYPE_LOCALV1: &str = "localv1";
const TEST_SHARED_VALUES_TYPE_REMOTEV1: &str = "remotev1";
const TEST_SHARED_VALUES_REMOTEV1_URL: &str = "TEST_SHARED_VALUES_REMOTEV1_URL";

pub type Results<T> = BTreeMap<String, T>;

/// Local implementation using a guarded BTreeMap as its datastore.
#[derive(Clone, Default)]
pub struct LocalV1 {
    num_waiters: Arc<AtomicUsize>,
    data: Arc<tokio::sync::Mutex<BTreeMap<String, String>>>,
    notification: Arc<tokio::sync::Mutex<BTreeMap<String, Arc<tokio::sync::Notify>>>>,
}

/// Remote implementation using Websockets for data passing.
#[derive(Clone)]
pub struct RemoteV1Client {
    url: url2::Url2,
    sender: Arc<holochain_websocket::WebsocketSender>,
    receiver: Arc<holochain_websocket::WebsocketReceiver>,
}

/// Remote implementation using Websockets for data passing.
#[derive(Clone)]
pub struct RemoteV1Server {
    url: url2::Url2,
    sender: Arc<holochain_websocket::WebsocketSender>,
    receiver: Arc<holochain_websocket::WebsocketReceiver>,
}

#[derive(Clone)]
pub enum SharedValues {
    LocalV1(LocalV1),
    RemoteV1Client(RemoteV1Client),
}

impl SharedValues {
    /// Returns a new MessageBus by respecting the environment variables:
    /// TEST_SHARED_VALUES_TYPE: can be either of
    /// - `localv1`: creates a message bus for in-process messaging
    /// - `remotev1`: creates a message bus for inter-process messaging. relies on another environment variable:
    ///     - TEST_SHARED_VALUES_REMOTEV1_URL: a URL for the remote endpoint to connect the message bus to
    pub async fn new_from_env() -> Fallible<Self> {
        let bus_type = std::env::var(TEST_SHARED_VALUES_TYPE)
            .unwrap_or(TEST_SHARED_VALUES_TYPE_LOCALV1.to_string());

        match bus_type.as_str() {
            TEST_SHARED_VALUES_TYPE_LOCALV1 => Ok(Self::LocalV1(LocalV1::default())),
            TEST_SHARED_VALUES_TYPE_REMOTEV1 => {
                let url_string = std::env::var(TEST_SHARED_VALUES_REMOTEV1_URL)?;
                let url = url2::Url2::try_parse(url_string)?;

                let (sender, receiver) =
                    holochain_websocket::connect(url.clone(), Default::default()).await?;

                Ok(Self::RemoteV1Client(RemoteV1Client {
                    url,
                    sender: Arc::new(sender),
                    receiver: Arc::new(receiver),
                }))
            }

            bus_type => {
                bail!("unknown message bus type: {bus_type}")
            }
        }
    }

    pub async fn num_waiters(&self) -> usize {
        match self {
            SharedValues::LocalV1(LocalV1 { num_waiters, .. }) => {
                num_waiters.load(Ordering::SeqCst)
            }

            _ => unimplemented!(),
        }
    }

    /// Gets all values that have a matching key prefix; waits for `min_results` to become available if specified.
    /// `wait_until` lets the caller decide under which conditions to accept the result, or otherwise keep waiting.
    ///
    /// Please look at the tests for usage examples.
    pub async fn get_pattern<T: for<'a> Deserialize<'a>, F>(
        &mut self,
        pattern: &str,
        mut maybe_wait_until: Option<F>,
    ) -> Fallible<Results<T>>
    where
        F: FnMut(&Results<T>) -> bool,
    {
        match self {
            SharedValues::LocalV1(localv1) => {
                loop {
                    let (notifier, notification);

                    // new scope so data_guard gets dropped before waiting for a notification
                    {
                        let data_guard = localv1.data.lock().await;

                        let mut results: Results<T> = Default::default();

                        for (key, value) in data_guard.iter() {
                            if key.matches(pattern).count() > 0 {
                                results.insert(key.to_string(), serde_json::from_str(&value)?);
                            }
                        }

                        if maybe_wait_until
                            .as_mut()
                            .map_or(true, |ref mut wait_until| wait_until(&results))
                        {
                            return Ok(results);
                        }

                        // get the notifier and start waiting on it while still holding the data_guard.
                        // this prevents a race between getting the notifier and a writer just writing something and sending notifications for it
                        localv1.num_waiters.fetch_add(1, Ordering::SeqCst);
                        notifier = localv1
                            .notification
                            .lock()
                            .await
                            .entry(pattern.to_string())
                            .or_default()
                            .clone();

                        notification = notifier.notified();
                    };

                    notification.await;

                    localv1.num_waiters.fetch_sub(1, Ordering::SeqCst);
                }
            }
            SharedValues::RemoteV1Client(_) => unimplemented!(),
        }
    }

    /// Puts the `value` for `key` and notifies any waiters if there are any.
    pub async fn put<T: Serialize + for<'a> Deserialize<'a>>(
        &mut self,
        key: String,
        value: T,
    ) -> Fallible<Option<T>> {
        match self {
            SharedValues::LocalV1(localv1) => {
                let mut data_guard = localv1.data.lock().await;

                let maybe_previous = if let Some(previous_serialized) =
                    data_guard.insert(key.clone(), serde_json::to_string(&value)?)
                {
                    Some(serde_json::from_str(&previous_serialized)?)
                } else {
                    None
                };

                for (pattern, notifier) in localv1.notification.lock().await.iter() {
                    if key.matches(pattern).count() > 0 {
                        eprintln!("{key} matched by {pattern}");
                        notifier.notify_waiters();
                    } else {
                        eprintln!("{key} not matched by {pattern}");
                    }
                }

                Ok(maybe_previous)
            }
            SharedValues::RemoteV1Client(_) => unimplemented!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use uuid::Uuid;

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shared_values_localv1_concurrent() {
        let mut values = SharedValues::LocalV1(LocalV1::default());

        let prefix = "something".to_string();
        let s = "we expect this back".to_string();

        let handle = {
            let prefix = prefix.clone();
            let s = s.clone();
            let mut values = values.clone();

            tokio::spawn({
                async move {
                    let got: String = values
                        .get_pattern(&prefix, Some(|results: &Results<_>| results.len() > 0))
                        .await
                        .unwrap()
                        .into_values()
                        .nth(0)
                        .unwrap();
                    eprintln!("got {got}");
                    assert_eq!(s, got);

                    got
                }
            })
        };

        // make sure the getter really comes first
        tokio::select! {
            _ = async {
                loop {
                    let num = values.num_waiters().await;
                    match num {
                        0 => tokio::time::sleep(Duration::from_millis(10)).await,
                        1 => { eprintln!("saw a getter!"); break },
                        _ => panic!("saw more than one waiter"),
                    };
                }
            } => {
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                panic!("didn't see a waiter");
            }
        };

        values.put(prefix, s).await.unwrap();

        if let Err(e) = handle.await {
            panic!("{:#?}", e);
        };
    }

    #[derive(Debug, Serialize, Deserialize, Clone)]
    struct AgentDummyInfo {
        id: Uuid,
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shared_values_localv1_simulate_agent_discovery() {
        let values = SharedValues::LocalV1(LocalV1::default());

        const PREFIX: &str = "agent_";

        let required_agents = 2;
        let num_agents = 2;

        let get_handle = {
            let mut values = values.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = async {
                        let all_agents: Results<AgentDummyInfo> = values.get_pattern(PREFIX, Some(|results: &Results<_>| results.len() >= num_agents)).await.unwrap();
                        assert!(required_agents <= all_agents.len());
                        assert!(all_agents.len() <= num_agents);
                        eprintln!("{} agents {all_agents:#?}", all_agents.len());
                    } => { }
                    _ = tokio::time::sleep(Duration::from_millis(50)) => { panic!("not enough agents"); }
                }
            })
        };

        let mut handles = vec![get_handle];
        for _ in 0..num_agents {
            let mut values = values.clone();

            let handle = tokio::spawn(async move {
                let agent_dummy_info = AgentDummyInfo {
                    id: uuid::Uuid::new_v4(),
                };
                values
                    .put(
                        format!("{PREFIX}{}", &agent_dummy_info.id),
                        agent_dummy_info,
                    )
                    .await
                    .unwrap();
            });
            handles.push(handle);
        }

        for handle in handles {
            if let Err(e) = handle.await {
                panic!("{:#?}", e);
            };
        }
    }
}
