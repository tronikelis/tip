use std::process;

pub struct DroppableChild(pub process::Child);

impl DroppableChild {
    pub fn new(child: process::Child) -> Self {
        Self(child)
    }
}

impl Drop for DroppableChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
