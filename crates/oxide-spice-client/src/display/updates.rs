//! Coalesced display notifications retain one value per independent surface.

use std::collections::BTreeMap;
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::Arc;

use tokio::sync::watch;

use crate::ClientError;

type SurfaceKey = (u8, u32);

struct Snapshot<T> {
    values: BTreeMap<SurfaceKey, Arc<T>>,
    latest: Option<SurfaceKey>,
}

#[derive(Clone)]
pub(crate) struct DisplaySender<T> {
    sender: watch::Sender<Snapshot<T>>,
}

#[derive(Clone)]
pub(crate) struct DisplayReceiver<T> {
    receiver: watch::Receiver<Snapshot<T>>,
    seen: BTreeMap<SurfaceKey, Arc<T>>,
    previous: Option<SurfaceKey>,
}

pub(crate) fn display_updates<T>() -> (DisplaySender<T>, DisplayReceiver<T>) {
    let (sender, receiver) = watch::channel(Snapshot {
        values: BTreeMap::new(),
        latest: None,
    });
    (
        DisplaySender { sender },
        DisplayReceiver {
            receiver,
            seen: BTreeMap::new(),
            previous: None,
        },
    )
}

impl<T> DisplaySender<T> {
    pub(crate) fn publish(&self, key: SurfaceKey, value: T) {
        self.sender.send_modify(|snapshot| {
            snapshot.values.insert(key, Arc::new(value));
            snapshot.latest = Some(key);
        });
    }

    pub(crate) fn remove(&self, key: SurfaceKey) {
        self.sender.send_modify(|snapshot| {
            snapshot.values.remove(&key);
            if snapshot.latest == Some(key) {
                snapshot.latest = None;
            }
        });
    }

    pub(crate) fn clear_channel(&self, channel_id: u8) {
        self.sender.send_modify(|snapshot| {
            snapshot
                .values
                .retain(|(channel, _), _| *channel != channel_id);
            if snapshot
                .latest
                .is_some_and(|(channel, _)| channel == channel_id)
            {
                snapshot.latest = None;
            }
        });
    }
}

impl<T: Clone> DisplayReceiver<T> {
    pub(crate) fn latest(&self) -> Option<T> {
        let snapshot = self.receiver.borrow();
        snapshot
            .latest
            .and_then(|key| snapshot.values.get(&key))
            .map(|value| (**value).clone())
    }

    pub(crate) async fn next(&mut self) -> Result<T, ClientError> {
        loop {
            let next = {
                let snapshot = self.receiver.borrow_and_update();
                self.seen.retain(|key, _| snapshot.values.contains_key(key));
                let boundary = self.previous.map_or(Unbounded, Excluded);
                // A continuously updating primary display must not starve a quieter display.
                snapshot
                    .values
                    .range((boundary, Unbounded))
                    .chain(snapshot.values.iter())
                    .find(|(key, value)| {
                        self.seen
                            .get(key)
                            .is_none_or(|seen| !Arc::ptr_eq(seen, value))
                    })
                    .map(|(key, value)| (*key, value.clone()))
            };
            if let Some((key, value)) = next {
                self.previous = Some(key);
                self.seen.insert(key, value.clone());
                return Ok((*value).clone());
            }
            self.receiver
                .changed()
                .await
                .map_err(|_| ClientError::TaskTerminated)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn coalesces_each_surface_without_starving_other_channels() {
        let (sender, mut first) = display_updates();
        let mut second = first.clone();
        sender.publish((0, 0), "old left");
        sender.publish((1, 0), "right");
        sender.publish((0, 0), "new left");
        assert_eq!(first.next().await.unwrap(), "new left");
        sender.publish((0, 0), "busy left");
        assert_eq!(first.next().await.unwrap(), "right");
        assert_eq!(first.next().await.unwrap(), "busy left");
        assert_eq!(second.next().await.unwrap(), "busy left");
        assert_eq!(second.next().await.unwrap(), "right");
        sender.publish((0, 1), "offscreen surface");
        sender.clear_channel(0);
        sender.publish((1, 0), "resized right");
        assert_eq!(first.next().await.unwrap(), "resized right");
        drop(sender);
        assert!(matches!(
            first.next().await,
            Err(ClientError::TaskTerminated)
        ));
    }
}
