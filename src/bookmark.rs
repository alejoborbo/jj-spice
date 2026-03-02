#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Bookmark {
    name: String,
}

impl Bookmark {
    pub fn new(name: String) -> Self {
        Self { name }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}
