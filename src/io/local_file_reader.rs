use std::fs;
use std::io::BufReader;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use log::debug;
use rand::prelude::*;

use super::*;

pub struct LocalFsReaderConfig {
    filter_ext: Option<std::ffi::OsString>,
    expect_dir: bool,
    dir_path: PathBuf,
    executor_partitions: Option<u64>,
}

impl LocalFsReaderConfig {
    /// Read all the files from a directory or a path.
    pub fn new<T: Into<PathBuf>>(path: T) -> LocalFsReaderConfig {
        LocalFsReaderConfig {
            filter_ext: None,
            expect_dir: true,
            dir_path: path.into(),
            executor_partitions: None,
        }
    }

    /// Only will read files with a given extension.
    pub fn filter_extension<T: Into<String>>(&mut self, extension: T) {
        self.filter_ext = Some(extension.into().into());
    }

    /// Default behaviour is to expect the directory to exist in every node,
    /// if it doesn't the executor will panic.
    pub fn expect_directory(&mut self, should_exist: bool) {
        self.expect_dir = should_exist;
    }

    /// Number of partitions to use per executor, to perform the load tasks,
    /// Ideally one executor is used per host with as many partitions as CPUs available.
    // TODO: profile this for actual sensible defaults
    pub fn num_partitions_per_executor(&mut self, num: u64) {
        self.executor_partitions = Some(num);
    }
}

impl ReaderConfiguration for LocalFsReaderConfig {
    fn make_reader(self, context: Arc<Context>) -> SerArc<dyn Rdd<Item = Vec<u8>>> {
        let reader = LocalFsReader::new(self, context.clone());
        let read_files = Fn!(|readers: Box<dyn Iterator<Item = LocalFsReaderSplit>>| {
            let mut files = Vec::new();
            for reader in readers {
                files.append(&mut reader.into_iter().collect::<Vec<_>>());
            }
            Box::new(files.into_iter()) as Box<dyn Iterator<Item = Vec<u8>>>
        });
        reader.map_partitions(read_files)
    }
}

/// Reads all files specified in a given directory from the local directory
/// on all executors on every worker node.
#[derive(Clone, Serialize, Deserialize)]
pub struct LocalFsReader {
    id: usize,
    path: PathBuf,
    is_single_file: bool,
    filter_ext: Option<std::ffi::OsString>,
    expect_dir: bool,
    executor_partitions: Option<u64>,
    #[serde(skip_serializing, skip_deserializing)]
    context: Arc<Context>,
}

impl LocalFsReader {
    pub fn new(config: LocalFsReaderConfig, context: Arc<Context>) -> Self {
        let LocalFsReaderConfig {
            dir_path,
            expect_dir,
            filter_ext,
            executor_partitions,
        } = config;

        let is_single_file = {
            let path: &Path = dir_path.as_ref();
            path.is_file()
        };

        LocalFsReader {
            id: context.new_rdd_id(),
            path: dir_path,
            is_single_file,
            filter_ext,
            expect_dir,
            executor_partitions,
            context,
        }
    }

    /// This function should be called once per host to come with the paralel workload.
    /// Is safe to recompute on failure though.
    fn load_local_files(&self) -> Result<Vec<Vec<PathBuf>>> {
        let mut total_size = 0_u64;
        if self.is_single_file {
            let size = fs::metadata(&self.path).map_err(Error::ReadFile)?.len();
            total_size += size;
            let files = vec![vec![self.path.clone()]];
            return Ok(files);
        }

        let mut num_partitions = self.get_executor_partitions();
        let mut files: Vec<(u64, PathBuf)> = vec![];
        // We compute std deviation incrementally to estimate a good breakpoint
        // of size per partition.
        let mut total_files = 0_u64;
        let mut k = 0;
        let mut ex = 0.0;
        let mut ex2 = 0.0;

        for (i, entry) in fs::read_dir(&self.path)
            .map_err(Error::ReadFile)?
            .enumerate()
        {
            let path = entry.map_err(Error::ReadFile)?.path();
            if path.is_file() {
                let is_proper_file = {
                    self.filter_ext.is_none()
                        || path.extension() == self.filter_ext.as_ref().map(|s| s.as_ref())
                };
                if !is_proper_file {
                    continue;
                }
                let size = fs::metadata(&path).map_err(Error::ReadFile)?.len();
                if i == 0 {
                    // assign first file size as reference sample
                    k = size;
                }
                // compute the necessary statistics
                ex += (size - k) as f32;
                ex2 += (size - k) as f32 * (size - k) as f32;
                total_size += size;
                total_files += 1;

                files.push((size, path));
            }
        }

        let file_size_mean = (total_size / total_files) as u64;
        let std_dev = ((ex2 - (ex * ex) / total_files as f32) / total_files as f32).sqrt();

        if total_files < num_partitions {
            // Coerce the number of partitions to the number of files
            num_partitions = total_files;
        }

        let avg_partition_size = (total_size / num_partitions) as u64;

        let partitions = self.assign_files_to_partitions(
            num_partitions,
            files,
            file_size_mean,
            avg_partition_size,
            std_dev,
        );

        Ok(partitions)
    }

    /// Assign files according to total avg partition size and file size.
    /// This should return a fairly balanced partition size.
    fn assign_files_to_partitions(
        &self,
        num_partitions: u64,
        files: Vec<(u64, PathBuf)>,
        file_size_mean: u64,
        avg_partition_size: u64,
        std_dev: f32,
    ) -> Vec<Vec<PathBuf>> {
        // Accept ~ 0.25 std deviations top from the average partition size
        // when assigning a file to a partition.
        let high_part_size_bound = (avg_partition_size + (std_dev * 0.25) as u64) as u64;

        debug!(
            "the average part size is {} with a high bound of {}",
            avg_partition_size, high_part_size_bound
        );
        debug!(
            "assigning files from local fs to partitions, file size mean: {}; std_dev: {}",
            file_size_mean, std_dev
        );

        let mut partitions = Vec::with_capacity(num_partitions as usize);
        let mut partition = Vec::with_capacity(0);
        let mut curr_part_size = 0_u64;
        let mut rng = rand::thread_rng();

        for (size, file) in files.into_iter() {
            if partitions.len() as u64 == num_partitions - 1 {
                partition.push(file);
                continue;
            }

            let new_part_size = curr_part_size + size;
            let larger_than_mean = rng.gen::<bool>();
            if (larger_than_mean && new_part_size < high_part_size_bound)
                || (!larger_than_mean && new_part_size <= avg_partition_size)
            {
                partition.push(file);
                curr_part_size = new_part_size;
            } else if size > avg_partition_size as u64 {
                partitions.push(partition);
                partitions.push(vec![file]);
                partition = vec![];
                curr_part_size = 0;
            } else {
                partitions.push(partition);
                partition = vec![file];
                curr_part_size = size;
            }
        }
        partitions.push(partition);
        let mut current_pos = partitions.len() - 1;
        while (partitions.len() as u64) < num_partitions {
            // If the number of specified partitions is relativelly equal to the number of files
            // or the file size of the last files is low skew can happen and there can be fewer
            // partitions than specified. This the number of partitions is actually the specified.
            if partitions.get(current_pos).unwrap().len() > 1 {
                // Only get elements from part as long as it has more than one element
                let last_part = partitions.get_mut(current_pos).unwrap().pop().unwrap();
                partitions.push(vec![last_part])
            } else if current_pos > 0 {
                current_pos -= 1;
            } else {
                break;
            }
        }
        partitions
    }

    fn get_executor_partitions(&self) -> u64 {
        if let Some(num) = self.executor_partitions {
            num
        } else {
            num_cpus::get() as u64
        }
    }
}

impl RddBase for LocalFsReader {
    fn get_rdd_id(&self) -> usize {
        self.id
    }

    fn get_context(&self) -> Arc<Context> {
        self.context.clone()
    }

    fn get_dependencies(&self) -> Vec<Dependency> {
        vec![]
    }

    fn is_pinned(&self) -> bool {
        true
    }

    fn preferred_locations(&self, split: Box<dyn Split>) -> Vec<Ipv4Addr> {
        // for a given split there is only one preferred location because this is pinned,
        // the preferred location is the host at which this split will be executed;
        let split = split.downcast_ref::<LocalFsReaderSplit>().unwrap();
        vec![split.host]
    }

    fn splits(&self) -> Vec<Box<dyn Split>> {
        let mut splits = Vec::with_capacity(self.context.address_map.len());
        for (idx, host) in self.context.address_map.iter().enumerate() {
            splits.push(Box::new(LocalFsReaderSplit {
                idx,
                host: *host.ip(),
                files: vec![],
            }) as Box<dyn Split>)
        }
        splits
    }

    default fn iterator_any(
        &self,
        split: Box<dyn Split>,
    ) -> Result<Box<dyn Iterator<Item = Box<dyn AnyData>>>> {
        Ok(Box::new(
            self.iterator(split)?
                .map(|x| Box::new(x) as Box<dyn AnyData>),
        ))
    }
}

impl Rdd for LocalFsReader {
    type Item = LocalFsReaderSplit;

    fn get_rdd(&self) -> Arc<dyn Rdd<Item = Self::Item>>
    where
        Self: Sized,
    {
        Arc::new(self.clone()) as Arc<dyn Rdd<Item = Self::Item>>
    }

    fn get_rdd_base(&self) -> Arc<dyn RddBase> {
        Arc::new(self.clone()) as Arc<dyn RddBase>
    }

    fn compute(&self, split: Box<dyn Split>) -> Result<Box<dyn Iterator<Item = Self::Item>>> {
        let split = split.downcast_ref::<LocalFsReaderSplit>().unwrap();
        let mut files_by_part = self.load_local_files().expect("failed to load local files");
        let idx = split.idx;
        let host = split.host;
        Ok(Box::new(
            files_by_part
                .into_iter()
                .map(move |files| LocalFsReaderSplit { files, idx, host }),
        ) as Box<dyn Iterator<Item = Self::Item>>)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LocalFsReaderSplit {
    files: Vec<PathBuf>,
    idx: usize,
    host: Ipv4Addr,
}

impl Split for LocalFsReaderSplit {
    fn get_index(&self) -> usize {
        self.idx
    }
}

impl Iterator for LocalFsReaderSplit {
    type Item = Vec<u8>;
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(path) = self.files.pop() {
            let mut file = fs::File::open(path).unwrap();
            let mut content = vec![];
            let mut reader = BufReader::new(file);
            reader.read_to_end(&mut content);
            Some(content)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_files() {
        let context = Context::new().unwrap();

        let mut loader = LocalFsReader {
            id: 0,
            path: "A".into(),
            is_single_file: false,
            filter_ext: None,
            expect_dir: true,
            executor_partitions: Some(4),
            context,
        };

        let file_size_mean = 1628;
        let avg_partition_size = 2850;
        let high_part_size_bound = 3945f32;

        // Skewed file sizes
        let files = vec![
            (500u64, "A".into()),
            (2000, "B".into()),
            (3900, "C".into()),
            (2000, "D".into()),
            (1000, "E".into()),
            (1500, "F".into()),
            (500, "G".into()),
        ];

        let files = loader.assign_files_to_partitions(
            4,
            files,
            file_size_mean,
            avg_partition_size,
            high_part_size_bound,
        );
        assert_eq!(files.len(), 4);

        // Even size and less files than parts
        loader.executor_partitions = Some(8);
        let files = vec![
            (500u64, "A".into()),
            (500, "B".into()),
            (500, "C".into()),
            (500, "D".into()),
            (500, "E".into()),
            (500, "F".into()),
        ];

        let files = loader.assign_files_to_partitions(
            8,
            files,
            file_size_mean,
            avg_partition_size,
            high_part_size_bound,
        );
        assert_eq!(files.len(), 6);
    }
}