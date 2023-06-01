use anyhow::{anyhow, Context, Result};
use discovery::DeviceId;
use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::Write,
    ops::RangeInclusive,
    path::{Path, PathBuf},
    sync::Mutex,
};
use tracing::info;

use crate::{proto::ReceivedRecords, RecordsSink};

struct Previous {
    path: PathBuf,
    range: RangeInclusive<u64>,
}

pub struct FilesRecordSink {
    base_path: PathBuf,
    previous: Mutex<HashMap<DeviceId, Previous>>,
}

impl FilesRecordSink {
    pub fn new(base_path: &Path) -> Self {
        Self {
            base_path: base_path.to_owned(),
            previous: Default::default(),
        }
    }

    fn device_path(&self, device_id: &DeviceId) -> PathBuf {
        self.base_path.join(&device_id.0)
    }

    fn append(&self, records: &ReceivedRecords, file_path: &PathBuf) -> Result<()> {
        // We'll usually be in a tokio context......
        let mut writing = OpenOptions::new()
            .append(true)
            .create(true)
            .open(file_path)
            .with_context(|| format!("Creating {:?}", &file_path))?;

        for record in records.iter() {
            writing.write(record.bytes())?;
        }

        Ok(())
    }

    fn create_new(&self, records: &ReceivedRecords) -> Result<PathBuf> {
        let range = records
            .range()
            .ok_or(anyhow!("No range on received records"))?;

        let device_path = self.device_path(&records.device_id);

        let sync_path = device_path.join(&records.sync_id);

        match std::fs::metadata(sync_path.clone()) {
            Ok(md) => {
                if !md.is_dir() {
                    return Err(anyhow!("Unexpected not-a-directory"));
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!("Creating {}", sync_path.display());
                std::fs::create_dir_all(sync_path.clone())
                    .with_context(|| format!("Creating sync path {:?}", &sync_path))?;
            }
            Err(e) => Err(e)?,
        }

        let file_path = sync_path.join(format!("{}.fkpb", range.start()));

        self.append(records, &file_path)?;

        Ok(file_path)
    }
}

impl RecordsSink for FilesRecordSink {
    fn write(&self, records: &ReceivedRecords) -> Result<()> {
        let range = records
            .range()
            .ok_or(anyhow!("No range on received records"))?;

        let mut previous = self.previous.lock().expect("Lock error");
        let consecutive = previous
            .get_mut(&records.device_id)
            .map(|p| (*range.start() == *p.range.end() + 1, p));

        match consecutive {
            Some((true, previous)) => {
                self.append(records, &previous.path)?;

                previous.range = *previous.range.start()..=*range.end()
            }
            Some((false, _)) | None => {
                let file_path = self.create_new(records)?;

                previous.insert(
                    records.device_id.clone(),
                    Previous {
                        path: file_path,
                        range,
                    },
                );
            }
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
        sync_id: String,
        device_id: DeviceId,
        records: Vec<NumberedRecord>,
        number: usize,
    }

    impl ReceivedRecordsBuilder {
        fn new() -> Self {
            Self {
                sync_id: "sync_id".to_owned(),
                device_id: DeviceId("device".to_owned()),
                records: Vec::new(),
                number: 0,
            }
        }

        fn build(self) -> ReceivedRecords {
            ReceivedRecords {
                sync_id: self.sync_id,
                device_id: self.device_id,
                records: self.records,
            }
        }

        fn first(self, number: usize) -> Self {
            Self {
                sync_id: self.sync_id,
                device_id: self.device_id,
                records: self.records,
                number,
            }
        }

        fn records(self, number: usize) -> Self {
            Self {
                sync_id: self.sync_id,
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
                sync_id: self.sync_id,
                device_id: self.device_id,
                records: self.records,
                number: self.number + number,
            }
        }
    }
}
