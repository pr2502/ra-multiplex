use tokio::sync::{watch, Mutex};

pub struct AsyncOnceCell<T> {
    send: Mutex<watch::Sender<Option<T>>>,
    recv: watch::Receiver<Option<T>>,
}

impl<T> Default for AsyncOnceCell<T> {
    fn default() -> Self {
        let (send, recv) = watch::channel(None);
        let send = Mutex::new(send);
        AsyncOnceCell { send, recv }
    }
}

impl<T: Clone> AsyncOnceCell<T> {
    pub async fn set(&self, val: T) -> Result<(), T> {
        let lock = self.send.lock().await;
        if let Some(val) = lock.borrow().as_ref() {
            // value was already set
            return Err(val.clone());
        };
        if lock.send(Some(val)).is_err() {
            unreachable!("BUG: channel can't close while we hold both ends");
        }
        Ok(())
    }

    /// waits for the value to arrive and then returns a copy
    pub async fn get(&self) -> T {
        let mut recv = self.recv.clone();
        loop {
            if let Some(val) = recv.borrow().as_ref() {
                return val.clone();
            }
            recv.changed().await.unwrap();
        }
    }
}
