pub struct Ring {
    pub items: Vec<u32>,
    head: usize,
}

impl Ring {
    pub fn new() -> Ring {
        Ring {
            items: Vec::new(),
            head: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.len() == self.head
    }
}
