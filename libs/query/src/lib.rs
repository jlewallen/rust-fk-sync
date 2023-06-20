pub mod device;
pub mod portal;

#[derive(Debug)]
pub struct BytesDownloaded {
    pub bytes_downloaded: u64,
    pub total_bytes: u64,
}

#[derive(Debug)]
pub struct BytesUploaded {
    pub bytes_uploaded: u64,
    pub total_bytes: u64,
}

impl BytesUploaded {
    pub fn completed(&self) -> bool {
        self.bytes_uploaded >= self.total_bytes
    }
}
