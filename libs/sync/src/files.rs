use anyhow::{anyhow, Context, Result};
use discovery::DeviceId;
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};

use crate::{server::ReceivedRecords, RecordsSink};

pub struct FilesRecordSink {
    base_path: PathBuf,
}

impl FilesRecordSink {
    pub fn new(base_path: &Path) -> Self {
        Self {
            base_path: base_path.to_owned(),
        }
    }

    fn device_path(&self, device_id: &DeviceId) -> Result<PathBuf> {
        let path = self.base_path.join(&device_id.0);

        std::fs::create_dir_all(path.clone())?;

        Ok(path)
    }
}

impl RecordsSink for FilesRecordSink {
    fn write(&self, records: &ReceivedRecords) -> Result<()> {
        let device_path = self
            .device_path(&records.device_id)
            .with_context(|| format!("Resolving device path {:?}", &records.device_id))?;
        let range = records
            .range()
            .ok_or(anyhow!("No range on received records"))?;

        let file_path = device_path.join(format!("{}.fkpb", range.start()));

        // We'll usually be in a tokio context......
        let mut writing = File::create(file_path.clone())
            .with_context(|| format!("Creating {:?}", &file_path))?;
        for record in records.iter() {
            writing.write(record.bytes())?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use discovery::DeviceId;
    use tempdir::TempDir;

    use crate::proto::{NumberedRecord, Record};

    use super::*;

    fn new_sink() -> Result<(FilesRecordSink, TempDir)> {
        let dir = TempDir::new("fk-tests-sync")?;

        Ok((FilesRecordSink::new(dir.path()), dir))
    }

    #[test]
    pub fn test_writes_initial_records() -> Result<()> {
        let (sink, _dir) = new_sink()?;

        sink.write(&builder().records(1000).build())?;

        Ok(())
    }

    #[test]
    pub fn test_writes_initial_records_with_gap() -> Result<()> {
        let (sink, _dir) = new_sink()?;

        sink.write(&builder().records(500).gap(10).records(490).build())?;

        Ok(())
    }

    #[test]
    pub fn test_writes_additional_records() -> Result<()> {
        let (sink, _dir) = new_sink()?;

        sink.write(&builder().records(1000).build())?;
        sink.write(&builder().first(1000).records(1000).build())?;

        Ok(())
    }

    #[test]
    pub fn test_writes_additional_records_with_gap() -> Result<()> {
        let (sink, _dir) = new_sink()?;

        sink.write(&builder().records(1000).build())?;
        sink.write(
            &builder()
                .first(1000)
                .records(500)
                .gap(10)
                .records(490)
                .build(),
        )?;

        Ok(())
    }

    #[test]
    pub fn test_writes_additional_records_that_fill_earlier_gap() -> Result<()> {
        let (sink, _dir) = new_sink()?;

        sink.write(&builder().records(1000).gap(100).records(900).build())?;
        sink.write(&builder().first(1000).records(100).build())?;

        Ok(())
    }

    fn builder() -> ReceivedRecordsBuilder {
        ReceivedRecordsBuilder::new()
    }

    pub struct ReceivedRecordsBuilder {
        device_id: DeviceId,
        records: Vec<NumberedRecord>,
        number: usize,
    }

    impl ReceivedRecordsBuilder {
        fn new() -> Self {
            Self {
                device_id: DeviceId("device".to_owned()),
                records: Vec::new(),
                number: 0,
            }
        }

        fn build(self) -> ReceivedRecords {
            ReceivedRecords {
                device_id: self.device_id,
                records: self.records,
            }
        }

        fn first(self, number: usize) -> Self {
            Self {
                device_id: self.device_id,
                records: self.records,
                number,
            }
        }

        fn records(self, number: usize) -> Self {
            Self {
                device_id: self.device_id,
                records: (0..number)
                    .into_iter()
                    .map(|n| NumberedRecord {
                        number: (n + self.number) as u64,
                        record: Record::new_all_zeros(256)
                            .into_delimited()
                            .expect("Error creating delimited test record."),
                    })
                    .collect(),
                number: self.number + number,
            }
        }

        fn gap(self, number: usize) -> Self {
            Self {
                device_id: self.device_id,
                records: self.records,
                number: self.number + number,
            }
        }
    }
}
