// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use crate::query_stage::PyQueryStage;
use crate::query_stage::QueryStage;
use crate::shuffle::{ShuffleReaderExec, ShuffleWriterExec};
use datafusion::error::Result;
use datafusion::physical_plan::coalesce_partitions::CoalescePartitionsExec;
use datafusion::physical_plan::repartition::RepartitionExec;
use datafusion::physical_plan::sorts::sort_preserving_merge::SortPreservingMergeExec;
use datafusion::physical_plan::{displayable, Partitioning};
use datafusion::physical_plan::{with_new_children_if_necessary, ExecutionPlan};
use log::debug;
use pyo3::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use uuid::Uuid;

#[pyclass(name = "ExecutionGraph", module = "datafusion_ray", subclass)]
pub struct PyExecutionGraph {
    pub graph: ExecutionGraph,
}

impl PyExecutionGraph {
    pub fn new(graph: ExecutionGraph) -> Self {
        Self { graph }
    }
}

#[pymethods]
impl PyExecutionGraph {
    /// Get a list of stages sorted by id
    pub fn get_query_stages(&self) -> Vec<PyQueryStage> {
        let mut stages = vec![];
        let max_id = self.graph.get_final_query_stage().id;
        for id in 0..=max_id {
            stages.push(PyQueryStage::from_rust(
                self.graph.query_stages.get(&id).unwrap().clone(),
            ));
        }
        stages
    }

    pub fn get_query_stage(&self, id: usize) -> PyResult<PyQueryStage> {
        if let Some(stage) = self.graph.query_stages.get(&id) {
            Ok(PyQueryStage::from_rust(stage.clone()))
        } else {
            todo!()
        }
    }

    pub fn get_final_query_stage(&self) -> PyQueryStage {
        PyQueryStage::from_rust(self.graph.get_final_query_stage())
    }
}

#[derive(Debug)]
pub struct ExecutionGraph {
    /// Query stages by id
    pub query_stages: HashMap<usize, Arc<QueryStage>>,
    id_generator: AtomicUsize,
}

impl Default for ExecutionGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecutionGraph {
    pub fn new() -> Self {
        Self {
            query_stages: HashMap::new(),
            id_generator: AtomicUsize::new(0),
        }
    }

    fn add_query_stage(&mut self, stage_id: usize, plan: Arc<dyn ExecutionPlan>) -> usize {
        let query_stage = QueryStage::new(stage_id, plan);
        self.query_stages.insert(stage_id, Arc::new(query_stage));
        stage_id
    }

    fn get_final_query_stage(&self) -> Arc<QueryStage> {
        // the final query stage is always the last to be created and
        // therefore has the highest id
        let mut max_id = 0;
        for k in self.query_stages.keys() {
            if *k > max_id {
                max_id = *k;
            }
        }
        self.query_stages.get(&max_id).unwrap().clone()
    }

    fn next_id(&self) -> usize {
        self.id_generator.fetch_add(1, Ordering::Relaxed)
    }
}

pub fn make_execution_graph(plan: Arc<dyn ExecutionPlan>) -> Result<ExecutionGraph> {
    let mut graph = ExecutionGraph::new();
    let root = generate_query_stages(plan, &mut graph)?;
    // We force the final stage to produce a single partition to return
    // to the driver. This might not suit ETL workloads.
    if root.properties().output_partitioning().partition_count() > 1 {
        let root = Arc::new(CoalescePartitionsExec::new(root));
        graph.add_query_stage(graph.next_id(), root);
    } else {
        graph.add_query_stage(graph.next_id(), root);
    }
    Ok(graph)
}

/// Convert a physical query plan into a distributed physical query plan by breaking the query
/// into query stages based on changes in partitioning.
fn generate_query_stages(
    plan: Arc<dyn ExecutionPlan>,
    graph: &mut ExecutionGraph,
) -> Result<Arc<dyn ExecutionPlan>> {
    // recurse down first
    let new_children: Vec<Arc<dyn ExecutionPlan>> = plan
        .children()
        .into_iter()
        .map(|x| generate_query_stages(x.clone(), graph))
        .collect::<Result<Vec<_>>>()?;
    let plan = with_new_children_if_necessary(plan, new_children)?;

    debug!("plan = {}", displayable(plan.as_ref()).one_line());
    debug!(
        "output_part = {:?}",
        plan.properties().output_partitioning()
    );

    let new_plan = if let Some(repart) = plan.as_any().downcast_ref::<RepartitionExec>() {
        match repart.partitioning() {
            &Partitioning::UnknownPartitioning(_) | &Partitioning::RoundRobinBatch(_) => {
                // just remove these
                Ok(repart.children()[0].clone())
            }
            partitioning_scheme => create_shuffle_exchange(
                plan.children()[0].clone(),
                graph,
                partitioning_scheme.clone(),
            ),
        }
    } else if plan
        .as_any()
        .downcast_ref::<CoalescePartitionsExec>()
        .is_some()
        || plan
            .as_any()
            .downcast_ref::<SortPreservingMergeExec>()
            .is_some()
    {
        let coalesce_input = plan.children()[0].clone();
        let partitioning_scheme = coalesce_input.properties().output_partitioning();
        let new_input = create_shuffle_exchange(
            coalesce_input.clone(),
            graph,
            partitioning_scheme.to_owned(),
        )?;
        with_new_children_if_necessary(plan, vec![new_input])
    } else {
        Ok(plan)
    }?;

    debug!("new_plan = {}", displayable(new_plan.as_ref()).one_line());
    debug!(
        "new_output_part = {:?}\n\n-------------------------\n\n",
        new_plan.properties().output_partitioning()
    );

    Ok(new_plan)
}

/// Create a shuffle exchange.
///
/// The plan is wrapped in a ShuffleWriteExec and added as a new query plan in the execution graph
/// and a ShuffleReaderExec is returned to replace the plan.
fn create_shuffle_exchange(
    plan: Arc<dyn ExecutionPlan>,
    graph: &mut ExecutionGraph,
    partitioning_scheme: Partitioning,
) -> Result<Arc<dyn ExecutionPlan>> {
    // introduce shuffle to produce one output partition
    let stage_id = graph.next_id();

    // create temp dir for stage shuffle files
    let temp_dir = create_temp_dir(stage_id)?;

    let shuffle_writer_input = plan.clone();
    let shuffle_writer: Arc<dyn ExecutionPlan> = Arc::new(ShuffleWriterExec::new(
        stage_id,
        shuffle_writer_input,
        partitioning_scheme.clone(),
        &temp_dir,
    ));

    debug!(
        "Created shuffle writer with output partitioning {:?}",
        shuffle_writer.properties().output_partitioning()
    );

    let stage_id = graph.add_query_stage(stage_id, shuffle_writer);
    // replace the plan with a shuffle reader
    Ok(Arc::new(ShuffleReaderExec::new(
        stage_id,
        plan.schema(),
        partitioning_scheme,
        &temp_dir,
    )))
}

fn create_temp_dir(stage_id: usize) -> Result<String> {
    let uuid = Uuid::new_v4();
    let temp_dir = format!("/tmp/ray-sql-{uuid}-stage-{stage_id}");
    debug!("Creating temp shuffle dir: {temp_dir}");
    std::fs::create_dir(&temp_dir)?;
    Ok(temp_dir)
}

#[cfg(test)]
mod test {
    use super::*;
    use datafusion::physical_plan::displayable;
    use datafusion::prelude::{ParquetReadOptions, SessionConfig, SessionContext};
    use pretty_assertions::assert_eq;
    use regex::Regex;
    use std::path::Path;
    use std::{env, fs};
    type TestResult<T> = std::result::Result<T, anyhow::Error>;

    #[tokio::test]
    async fn test_q1() -> TestResult<()> {
        do_test(1).await
    }

    #[tokio::test]
    async fn test_q2() -> TestResult<()> {
        do_test(2).await
    }

    #[tokio::test]
    async fn test_q3() -> TestResult<()> {
        do_test(3).await
    }

    #[tokio::test]
    async fn test_q4() -> TestResult<()> {
        do_test(4).await
    }

    #[tokio::test]
    async fn test_q5() -> TestResult<()> {
        do_test(5).await
    }

    #[tokio::test]
    async fn test_q6() -> TestResult<()> {
        do_test(6).await
    }

    #[ignore = "non-deterministic IN clause"]
    #[tokio::test]
    async fn test_q7() -> TestResult<()> {
        do_test(7).await
    }

    #[tokio::test]
    async fn test_q8() -> TestResult<()> {
        do_test(8).await
    }

    #[tokio::test]
    async fn test_q9() -> TestResult<()> {
        do_test(9).await
    }

    #[tokio::test]
    async fn test_q10() -> TestResult<()> {
        do_test(10).await
    }

    #[tokio::test]
    async fn test_q11() -> TestResult<()> {
        do_test(11).await
    }

    #[ignore = "non-deterministic IN clause"]
    #[tokio::test]
    async fn test_q12() -> TestResult<()> {
        do_test(12).await
    }

    #[tokio::test]
    async fn test_q13() -> TestResult<()> {
        do_test(13).await
    }

    #[tokio::test]
    async fn test_q14() -> TestResult<()> {
        do_test(14).await
    }

    #[ignore]
    #[tokio::test]
    async fn test_q15() -> TestResult<()> {
        do_test(15).await
    }

    // This test is ignored because there is some non-determinism
    // in a part of the plan, see
    // https://github.com/edmondop/datafusion-ray/actions/runs/11180062292/job/31080996808"
    #[ignore = "non-deterministic IN clause"]
    #[tokio::test]
    async fn test_q16() -> TestResult<()> {
        do_test(16).await
    }

    #[tokio::test]
    async fn test_q17() -> TestResult<()> {
        do_test(17).await
    }

    #[tokio::test]
    async fn test_q18() -> TestResult<()> {
        do_test(18).await
    }

    #[ignore = "non-deterministic IN clause"]
    #[tokio::test]
    async fn test_q19() -> TestResult<()> {
        do_test(19).await
    }

    #[tokio::test]
    async fn test_q20() -> TestResult<()> {
        do_test(20).await
    }

    #[tokio::test]
    async fn test_q21() -> TestResult<()> {
        do_test(21).await
    }

    #[tokio::test]
    async fn test_q22() -> TestResult<()> {
        do_test(22).await
    }

    async fn do_test(n: u8) -> TestResult<()> {
        let tpch_path_env_var = "TPCH_DATA_PATH";
        let data_path = env::var(tpch_path_env_var)
            .unwrap_or_else(|_| panic!("Environment variable {} not found", tpch_path_env_var));

        let file = format!("testdata/queries/q{n}.sql");
        let sql = fs::read_to_string(&file)?;
        let config = SessionConfig::new().with_target_partitions(2);
        let ctx = SessionContext::new_with_config(config);
        let tables = &[
            "customer", "lineitem", "nation", "orders", "part", "partsupp", "region", "supplier",
        ];
        for table in tables {
            ctx.register_parquet(
                table,
                &format!("{data_path}/{table}.parquet"),
                ParquetReadOptions::default(),
            )
            .await?;
        }
        let mut output = String::new();

        let df = ctx.sql(&sql).await?;

        let plan = df.clone().into_optimized_plan()?;
        output.push_str(&format!(
            "DataFusion Logical Plan\n=======================\n\n{}\n\n",
            plan.display_indent()
        ));

        let plan = df.create_physical_plan().await?;
        output.push_str(&format!(
            "DataFusion Physical Plan\n========================\n\n{}\n",
            displayable(plan.as_ref()).indent(false)
        ));

        output.push_str("DataFusion Ray Distributed Plan\n===========\n\n");
        let graph = make_execution_graph(plan)?;
        for id in 0..=graph.get_final_query_stage().id {
            let query_stage = graph.query_stages.get(&id).unwrap();
            output.push_str(&format!(
                "Query Stage #{id} ({} -> {}):\n{}\n",
                query_stage.get_input_partition_count(),
                query_stage.get_output_partition_count(),
                displayable(query_stage.plan.as_ref()).indent(false)
            ));
        }

        // Remove Parquet file group information since it will vary between CI/CD and local
        let re = Regex::new(r"file_groups=\{.*}")?;
        let cleaned_output = re.replace_all(output.as_str(), "file_groups={ ... }");

        let expected_file = format!("testdata/expected-plans/q{n}.txt");
        if !Path::new(&expected_file).exists() {
            fs::write(&expected_file, &*cleaned_output)?;
        }
        let expected_plan = fs::read_to_string(&expected_file)?;

        assert_eq!(expected_plan, cleaned_output);
        Ok(())
    }
}
