use std::{collections::HashMap, sync::Arc};

use tokio::{
    sync::{Mutex, watch},
    task::JoinHandle,
};

#[allow(dead_code)]
#[derive(Clone)]
pub struct Server {
    services: Arc<Mutex<HashMap<String, ServiceHandle>>>,
}

#[allow(dead_code)]
pub struct ServiceHandle {
    pub shutdown: watch::Sender<bool>,
    pub task: JoinHandle<()>,
}

#[allow(dead_code)]
#[derive(Debug)]
pub enum AddServiceError {
    AlreadyExists,
}

impl Server {
    pub fn new() -> Self {
        Self {
            services: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[allow(dead_code)]
    pub async fn add_service(
        &self,
        name: String,
        service: ServiceHandle,
    ) -> Result<(), AddServiceError> {
        let mut guard = self.services.lock().await;
        if guard.contains_key(&name) {
            return Err(AddServiceError::AlreadyExists);
        }
        guard.insert(name, service);
        Ok(())
    }

    pub async fn upsert_service(&self, name: String, service: ServiceHandle) {
        let old = {
            let mut guard = self.services.lock().await;
            guard.insert(name, service)
        };

        if let Some(old) = old {
            let _ = old.shutdown.send(true);
            let _ = old.task.await;
        }
    }

    pub async fn remove_service(&self, name: &str) {
        let old = {
            let mut guard = self.services.lock().await;
            guard.remove(name)
        };

        if let Some(old) = old {
            let _ = old.shutdown.send(true);
            let _ = old.task.await;
        }
    }

    pub async fn stop_all(&self) {
        let services = {
            let mut guard = self.services.lock().await;
            std::mem::take(&mut *guard)
        };

        for (_, handle) in services {
            let _ = handle.shutdown.send(true);
            let _ = handle.task.await;
        }
    }
}
