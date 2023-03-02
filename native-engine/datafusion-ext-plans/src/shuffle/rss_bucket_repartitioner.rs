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

//! Defines the rss bucket shuffle repartitioner

use crate::shuffle::{evaluate_hashes, evaluate_partition_ids, ShuffleRepartitioner};
use async_trait::async_trait;
use blaze_commons::{jni_call, jni_delete_local_ref, jni_new_direct_byte_buffer};
use datafusion::arrow::array::*;
use datafusion::arrow::datatypes::*;
use datafusion::arrow::error::Result as ArrowResult;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::Result;
use datafusion::execution::context::TaskContext;
use datafusion::execution::memory_manager::ConsumerType;
use datafusion::execution::runtime_env::RuntimeEnv;
use datafusion::execution::{MemoryConsumer, MemoryConsumerId, MemoryManager};
use datafusion::physical_plan::metrics::BaselineMetrics;
use datafusion::physical_plan::Partitioning;
use datafusion_ext_commons::array_builder::{
    builder_extend, make_batch, new_array_builders,
};
use datafusion_ext_commons::io::write_one_batch;
use futures::lock::Mutex;
use itertools::Itertools;
use jni::objects::{GlobalRef, JObject};
use std::fmt;
use std::fmt::{Debug, Formatter};
use std::io::Cursor;
use std::sync::Arc;

pub struct RssBucketShuffleRepartitioner {
    id: MemoryConsumerId,
    buffered_partitions: Mutex<Vec<PartitionBuffer>>,
    partitioning: Partitioning,
    num_output_partitions: usize,
    runtime: Arc<RuntimeEnv>,
    metrics: BaselineMetrics,
}

impl RssBucketShuffleRepartitioner {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        partition_id: usize,
        rss_partition_writer: GlobalRef,
        schema: SchemaRef,
        partitioning: Partitioning,
        metrics: BaselineMetrics,
        context: Arc<TaskContext>,
    ) -> Self {
        let num_output_partitions = partitioning.partition_count();
        let runtime = context.runtime_env();
        let batch_size = context.session_config().batch_size();
        let repartitioner = Self {
            id: MemoryConsumerId::new(partition_id),
            buffered_partitions: Mutex::new(
                (0..num_output_partitions)
                    .into_iter()
                    .map(|_| PartitionBuffer::new(
                            schema.clone(),
                            batch_size,
                            rss_partition_writer.clone(),
                    ))
                    .collect::<Vec<_>>(),
            ),
            partitioning,
            num_output_partitions,
            runtime,
            metrics,
        };
        repartitioner.runtime.register_requester(&repartitioner.id);
        repartitioner
    }
}

#[async_trait]
impl ShuffleRepartitioner for RssBucketShuffleRepartitioner {
    fn name(&self) -> &str {
        "bucket rss repartitioner"
    }

    async fn insert_batch(&self, input: RecordBatch) -> Result<()> {
        let mem_increase = input.get_array_memory_size();
        self.metrics.mem_used().add(mem_increase);
        self.grow(mem_increase);
        self.try_grow(0).await?;

        // compute partition ids
        let num_output_partitions = self.num_output_partitions;
        let hashes = evaluate_hashes(&self.partitioning, &input)?;
        let partition_ids = evaluate_partition_ids(&hashes, num_output_partitions);

        // count each partition size
        let mut partition_counters = vec![0usize; num_output_partitions];
        for &partition_id in &partition_ids {
            partition_counters[partition_id as usize] += 1
        }

        // accumulate partition counters into partition ends
        let mut partition_ends = partition_counters;
        let mut accum = 0;
        partition_ends.iter_mut().for_each(|v| {
            *v += accum;
            accum = *v;
        });

        // calculate shuffled partition ids
        let mut shuffled_partition_ids = vec![0usize; input.num_rows()];
        for (index, &partition_id) in partition_ids.iter().enumerate().rev() {
            partition_ends[partition_id as usize] -= 1;
            let end = partition_ends[partition_id as usize];
            shuffled_partition_ids[end] = index;
        }

        // after calculating, partition ends become partition starts
        let mut partition_starts = partition_ends;
        partition_starts.push(input.num_rows());

        for (partition_id, (&start, &end)) in partition_starts
            .iter()
            .tuple_windows()
            .enumerate()
            .filter(|(_, (start, end))| start < end)
        {
            let mut buffered_partitions = self.buffered_partitions.lock().await;
            let output = &mut buffered_partitions[partition_id];

            if end - start < output.rss_batch_size {
                output.append_rows(
                    input.columns(),
                    &shuffled_partition_ids[start..end],
                    partition_id,
                )?;
            } else {
                // for bigger slice, we can use column based operation
                // to build batches and directly append to output.
                // so that we can get rid of column <-> row conversion.
                let indices = PrimitiveArray::from_iter(
                    shuffled_partition_ids[start..end]
                        .iter()
                        .map(|&idx| idx as u64),
                );
                let batch = RecordBatch::try_new(
                    input.schema(),
                    input
                        .columns()
                        .iter()
                        .map(|c| arrow::compute::take(c, &indices, None))
                        .collect::<ArrowResult<Vec<ArrayRef>>>()?,
                )?;
                output.append_batch(batch, partition_id)?;
            }
            drop(buffered_partitions);
        }
        Ok(())
    }

    async fn shuffle_write(&self) -> Result<()> {
        let mut buffered_partitions = self.buffered_partitions.lock().await;
        for i in 0..self.num_output_partitions {
            buffered_partitions[i].flush_to_rss(i)?;
        }
        Ok(())
    }
}

impl Debug for RssBucketShuffleRepartitioner {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("RssBucketRepartitioner")
            .field("id", &self.id())
            .field("memory_used", &self.mem_used())
            .finish()
    }
}

#[async_trait]
impl MemoryConsumer for RssBucketShuffleRepartitioner {
    fn name(&self) -> String {
        "rss bucket repartitioner".to_string()
    }

    fn id(&self) -> &MemoryConsumerId {
        &self.id
    }

    fn memory_manager(&self) -> Arc<MemoryManager> {
        self.runtime.memory_manager.clone()
    }

    fn type_(&self) -> &ConsumerType {
        &ConsumerType::Requesting
    }

    async fn spill(&self) -> Result<usize> {
        let mut buffered_partitions = self.buffered_partitions.lock().await;
        if buffered_partitions.len() == 0 {
            return Ok(0);
        }
        for i in 0..self.num_output_partitions {
            buffered_partitions[i].flush_to_rss(i)?;
        }
        Ok(self.metrics.mem_used().set(0))
    }

    fn mem_used(&self) -> usize {
        self.metrics.mem_used().value()
    }
}

impl Drop for RssBucketShuffleRepartitioner {
    fn drop(&mut self) {
        self.runtime.drop_consumer(self.id(), self.mem_used());
    }
}

struct PartitionBuffer {
    rss_partition_writer: GlobalRef,
    schema: SchemaRef,
    active: Vec<Box<dyn ArrayBuilder>>,
    num_active_rows: usize,
    rss_batch_size: usize,
}

impl PartitionBuffer {
    fn new(
        schema: SchemaRef,
        batch_size: usize,
        rss_partition_writer: GlobalRef,
    ) -> Self {
        // use smaller batch size for rss to trigger more flushes
        let rss_batch_size = batch_size / (batch_size as f64 + 1.0).log2() as usize;
        Self {
            rss_partition_writer,
            schema,
            active: vec![],
            num_active_rows: 0,
            rss_batch_size,
        }
    }

    fn append_rows(
        &mut self,
        columns: &[ArrayRef],
        indices: &[usize],
        partition_id: usize,
    ) -> Result<()> {
        let mut start = 0;

        while start < indices.len() {
            // lazy init because some partition may be empty
            if self.active.is_empty() {
                self.active = new_array_builders(&self.schema, self.rss_batch_size);
            }

            let extend_len = (indices.len() - start)
                .min(self.rss_batch_size.saturating_sub(self.num_active_rows));
            self.active
                .iter_mut()
                .zip(columns)
                .for_each(|(builder, column)| {
                    builder_extend(
                        builder,
                        column,
                        &indices[start..][..extend_len],
                        column.data_type(),
                    );
                });
            self.num_active_rows += extend_len;
            if self.num_active_rows >= self.rss_batch_size {
                self.flush_to_rss(partition_id)?;
            }
            start += extend_len;
        }
        Ok(())
    }

    /// append a whole batch directly to staging
    /// this will break the appending order when mixing with append_rows(), but
    /// it does not affect the shuffle output result.
    fn append_batch(&mut self, batch: RecordBatch, partition_id: usize) -> Result<()> {
        write_batch_to_rss(self.rss_partition_writer.as_obj(), partition_id, &batch)
    }

    /// flush active data into rss
    fn flush_to_rss(&mut self, partition_id: usize) -> Result<()> {
        if self.num_active_rows == 0 {
            return Ok(());
        }
        let active = std::mem::take(&mut self.active);
        self.num_active_rows = 0;

        let batch = make_batch(self.schema.clone(), active)?;
        write_batch_to_rss(self.rss_partition_writer.as_obj(), partition_id, &batch)?;
        Ok(())
    }
}

fn write_batch_to_rss(
    rss_partition_writer: JObject,
    partition_id: usize,
    batch: &RecordBatch,
) -> Result<()> {
    let mut data = vec![];

    write_one_batch(batch, &mut Cursor::new(&mut data), true)?;
    let data_len = data.len();
    let buf = jni_new_direct_byte_buffer!(&mut data)?;
    jni_call!(
        BlazeRssPartitionWriterBase(rss_partition_writer).write(
            partition_id as i32,
            buf,
            data_len as i32,
        ) -> ())?;
    jni_delete_local_ref!(buf.into())?;
    Ok(())
}