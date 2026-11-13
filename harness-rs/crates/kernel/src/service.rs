use std::any::{type_name, Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::{KernelError, Result};

/// Type-keyed container for long-lived services.
///
/// Services are stored as `Arc<dyn Any + Send + Sync>` and handed back as
/// `Arc<T>`, so callers can share state without the registry owning the
/// service's concrete type ahead of time.
#[derive(Default)]
pub struct ServiceRegistry {
    services: RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

impl ServiceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one service per concrete type.
    pub fn insert<T>(&self, service: T) -> Result<()>
    where
        T: Any + Send + Sync,
    {
        let mut services = self.services.write().map_err(|_| KernelError::Poisoned)?;
        let type_id = TypeId::of::<T>();
        if services.contains_key(&type_id) {
            return Err(KernelError::DuplicateService(type_name::<T>()));
        }
        services.insert(type_id, Arc::new(service));
        Ok(())
    }

    pub fn get<T>(&self) -> Option<Arc<T>>
    where
        T: Any + Send + Sync,
    {
        let services = self.services.read().ok()?;
        services.get(&TypeId::of::<T>())?.clone().downcast::<T>().ok()
    }

    pub fn contains<T>(&self) -> bool
    where
        T: Any + Send + Sync,
    {
        self.get::<T>().is_some()
    }

    pub fn remove<T>(&self) -> bool
    where
        T: Any + Send + Sync,
    {
        self.services
            .write()
            .map(|mut services| services.remove(&TypeId::of::<T>()).is_some())
            .unwrap_or(false)
    }

    pub fn len(&self) -> usize {
        self.services.read().map(|services| services.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_services_by_concrete_type() {
        let registry = ServiceRegistry::new();
        registry.insert(42_u32).unwrap();
        registry.insert("hello".to_string()).unwrap();

        assert_eq!(*registry.get::<u32>().unwrap(), 42);
        assert_eq!(registry.get::<String>().unwrap().as_str(), "hello");
        assert!(registry.get::<u64>().is_none());
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn rejects_duplicate_service_types() {
        let registry = ServiceRegistry::new();
        registry.insert(1_u8).unwrap();
        assert!(matches!(
            registry.insert(2_u8),
            Err(KernelError::DuplicateService(_))
        ));
        assert_eq!(*registry.get::<u8>().unwrap(), 1);
    }

    #[test]
    fn remove_drops_the_service() {
        let registry = ServiceRegistry::new();
        registry.insert(7_i64).unwrap();
        assert!(registry.remove::<i64>());
        assert!(!registry.remove::<i64>());
        assert!(registry.is_empty());
    }
}
