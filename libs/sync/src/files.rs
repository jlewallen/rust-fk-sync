use anyhow::{anyhow, Context, Result};
use itertools::*;
use quick_protobuf::Reader;
use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::Write,
    ops::RangeInclusive,
    path::{Path, PathBuf},
    sync::Mutex,
};
use tracing::*;

use crate::{
    proto::{Identity, ReceivedRecords, Record},
    RecordsSink,
};
use discovery::DeviceId;
use protos::FileMeta;

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
            writing.write(record.to_delimited()?.bytes())?;
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

    #[allow(dead_code)]
    fn get_device_ids(&self) -> Result<Vec<DeviceId>> {
        let previous = self.previous.lock().expect("Lock error");
        Ok(previous.keys().map(|d| d.clone()).collect())
    }

    fn join_files(
        &self,
        sync_id: &String,
        identity: &Identity,
        files: Vec<RecordsFile>,
    ) -> Result<i64> {
        assert_eq!(files.get(0).map(|f| f.head), Some(0));

        let device_path = self.device_path(&identity.device_id);
        let path = device_path.join(format!("{}.fkpb", sync_id));

        let mut writing = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("Creating {:?}", &path))?;

        let mut written = 0;

        for file in files.iter() {
            let mut skipping = written - file.head;
            debug!("{:?} Records={:?} Skipped={:?}", file, written, skipping);
            assert!(skipping >= 0);

            let mut reader = Reader::from_file(&file.path)?;
            while let Some(record) = reader.read(|r, b| {
                if r.is_eof() {
                    Ok(None)
                } else {
                    Ok(Some(r.read_bytes(b)?))
                }
            })? {
                if skipping == 0 {
                    let record = Record::Undelimited(record.to_vec());
                    let record = record.to_delimited()?;
                    writing.write(record.bytes())?;
                    written += 1;
                } else {
                    skipping -= 1;
                }
            }
        }

        info!("{} Flushed {} records", path.display(), written);

        Ok(written)
    }

    fn write_identity(
        &self,
        sync_id: &String,
        identity: &Identity,
        total_records: i64,
    ) -> Result<()> {
        let device_path = self.device_path(&identity.device_id);
        let path = device_path.join(format!("{}.fkpb.json", sync_id));

        let writing = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("Creating {:?}", &path))?;

        let mut headers = identity.to_headers_map();
        // Yes, this appears to be "last record number" instead of "total number
        // of records" based on the firmware.
        headers.insert(
            "Fk-Blocks".to_owned(),
            format!("{},{}", 0, total_records - 1),
        );
        headers.insert("Fk-Type".to_owned(), "data".to_owned());

        let fm = FileMeta { headers };

        serde_json::to_writer(writing, &fm)?;

        info!("{} Wrote", &path.display());

        Ok(())
    }
}

#[derive(Debug)]
struct RecordsFile {
    path: PathBuf,
    head: i64,
}

impl RecordsFile {
    fn new(path: &PathBuf) -> Result<Self> {
        let name = path.file_name().expect("No file name on path");
        let head = name
            .to_os_string()
            .into_string()
            .map_err(|_| anyhow!("Quirky file name"))?
            .split(".")
            .next()
            .map(|v| Ok(v.parse()?))
            .unwrap_or(Err(anyhow!("Malformed record file name")))?;

        Ok(Self {
            path: path.clone(),
            head,
        })
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

    fn flush(&self, sync_id: String, identity: Identity) -> Result<()> {
        let device_path = self.device_path(&identity.device_id);
        let sync_path = device_path.join(&sync_id);

        info!("flushing {:?}", &sync_path);

        let files: Vec<_> = std::fs::read_dir(sync_path)?
            .map(|entry| Ok(RecordsFile::new(&entry?.path())?))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .sorted_unstable_by_key(|r| r.head)
            .collect();

        let total_records = self.join_files(&sync_id, &identity, files)?;

        self.write_identity(&sync_id, &identity, total_records)?;

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
                            .to_delimited()
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
