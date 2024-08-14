// Copyright 2022 The Blaze Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![feature(get_mut_unchecked)]
#![feature(adt_const_params)]
#![feature(offset_of)]
#![feature(async_closure)]

use std::{collections::HashMap, sync::Arc};

use datafusion::error::{DataFusionError, Result};
use hdfs_native::Client;
use hdfs_native_object_store::HdfsObjectStore;
use once_cell::sync::OnceCell;

// execution plan implementations
pub mod agg_exec;
pub mod broadcast_join_build_hash_map_exec;
pub mod broadcast_join_exec;
pub mod debug_exec;
pub mod empty_partitions_exec;
pub mod expand_exec;
pub mod ffi_reader_exec;
pub mod filter_exec;
pub mod generate_exec;
pub mod hash_join_exec;
pub mod ipc_reader_exec;
pub mod ipc_writer_exec;
pub mod limit_exec;
pub mod parquet_exec;
pub mod parquet_sink_exec;
pub mod project_exec;
pub mod rename_columns_exec;
pub mod rss_shuffle_writer_exec;
pub mod shuffle_writer_exec;
pub mod sort_exec;
pub mod sort_merge_join_exec;
pub mod window_exec;

// memory management
pub mod memmgr;

// helper modules
pub mod agg;
pub mod common;
pub mod generate;
pub mod joins;
mod shuffle;
pub mod window;

pub fn get_hdfs_object_store() -> Result<Arc<HdfsObjectStore>> {
    static HDFS_OBJECT_STORE: OnceCell<Arc<HdfsObjectStore>> = OnceCell::new();
    let config: HashMap<String, String> = HashMap::from([
        (
            "dfs.ha.namenodes.blaze-test".to_string(),
            "nn1,nn2,nn3".to_string(),
        ),
        (
            "dfs.namenode.rpc-address.blaze-test.nn1".to_string(),
            "10.108.234.143:8020".to_string(),
        ),
        (
            "dfs.namenode.rpc-address.blaze-test.nn2".to_string(),
            "10.14.35.152:8020".to_string(),
        ),
        (
            "dfs.namenode.rpc-address.blaze-test.nn3".to_string(),
            "10.14.35.231:8020".to_string(),
        ),
    ]);
    Ok(HDFS_OBJECT_STORE
        .get_or_try_init(|| {
            Ok::<_, DataFusionError>(Arc::new(HdfsObjectStore::new(
                Client::new_with_config("hdfs://blaze-test", config.clone())
                    .map_err(|e| DataFusionError::External(Box::new(e)))?,
            )))
        })?
        .clone())
}
