use tokio::sync::{Notify, OnceCell, SetError};

pub struct WaitCell<T> {
    value: OnceCell<T>,
    notif: Notify,
}

impl<T> Default for WaitCell<T> {
    fn default() -> Self {
        WaitCell {
            value: OnceCell::new(),
            notif: Notify::new(),
        }
    }
}

impl<T> WaitCell<T> {
    pub async fn set(&self, val: T) -> Result<(), SetError<T>> {
        let res = self.value.set(val);
        self.notif.notify_waiters();
        res
    }

    /// Waits for the value to arrive and then returns a copy
    pub async fn wait(&self) -> &T {
        loop {
            if let Some(val) = self.value.get() {
                return val;
            }
            self.notif.notified().await;
        }
    }
}
