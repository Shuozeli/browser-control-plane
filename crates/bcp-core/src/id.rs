use std::collections::VecDeque;
use std::sync::Mutex;

pub trait IdGenerator: Send + Sync {
    fn next_id(&self, prefix: &str) -> String;
}

#[derive(Debug, Default)]
pub struct UuidIdGenerator;

impl IdGenerator for UuidIdGenerator {
    fn next_id(&self, prefix: &str) -> String {
        format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
    }
}

#[derive(Debug)]
pub struct FakeIdGenerator {
    ids: Mutex<VecDeque<String>>,
}

impl FakeIdGenerator {
    pub fn new(ids: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            ids: Mutex::new(ids.into_iter().map(Into::into).collect()),
        }
    }
}

impl IdGenerator for FakeIdGenerator {
    fn next_id(&self, prefix: &str) -> String {
        self.ids
            .lock()
            .expect("fake id generator mutex poisoned")
            .pop_front()
            .unwrap_or_else(|| format!("{prefix}_fake"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_id_generator_returns_seeded_ids_then_prefixed_fallback() {
        // Arrange
        let ids = FakeIdGenerator::new(["lease_1"]);

        // Act
        let first = ids.next_id("lease");
        let second = ids.next_id("fence");

        // Assert
        assert_eq!(first, "lease_1");
        assert_eq!(second, "fence_fake");
    }
}
