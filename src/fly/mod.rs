use std::path::Path;
use std::time::Instant;

use crate::analyze;
use crate::cli::{AnalyzeArgs, CountArgs, FlyArgs};
use crate::count;
use crate::kiteops::infer_sample_labels;
use crate::stats::print_simple_test_report;

pub fn run_fly(args: &FlyArgs) -> anyhow::Result<()> {
    let started = Instant::now();
    std::fs::create_dir_all(&args.output_dir)?;
    let db_dir = Path::new(&args.output_dir).join("databases");
    let analysis_dir = Path::new(&args.output_dir).join("analysis");
    std::fs::create_dir_all(&db_dir)?;
    std::fs::create_dir_all(&analysis_dir)?;

    let labels = infer_sample_labels(&args.inputs, args.labels.as_deref().unwrap_or(&[]))?;
    let mut kite_inputs = Vec::with_capacity(args.inputs.len());

    for (index, input) in args.inputs.iter().enumerate() {
        let output_path = db_dir.join(format!("{}.kite", labels[index]));
        eprintln!(
            "[kite fly] count {}/{} -> {}",
            index + 1,
            args.inputs.len(),
            output_path.display()
        );
        let count_args = CountArgs {
            k: args.k,
            input: input.clone(),
            output: output_path.to_string_lossy().to_string(),
            threads: args.threads,
            partitions: 0,
            chunk_kmers: 8_000_000,
            memory: args.memory.clone(),
            canonical: true,
            progress: args.progress,
            test_mode: false,
            mode: crate::cli::CountMode::Fast,
            buffer: 16,
        };
        count::run_count(&count_args)?;
        kite_inputs.push(output_path.to_string_lossy().to_string());
    }

    let analyze_args = AnalyzeArgs {
        inputs: kite_inputs,
        output_dir: analysis_dir.to_string_lossy().to_string(),
        labels: Some(labels.clone()),
        top_variable_kmers: args.top_variable_kmers,
        correlation: args.correlation,
        embedding: args.embedding,
        heatmap_kmeans: args.heatmap_kmeans,
        heatmap_seed: args.heatmap_seed,
        cluster_samples: args.cluster_samples,
        progress: args.progress,
        test_mode: false,
    };
    analyze::run_analyze(&analyze_args)?;
    if args.test_mode {
        print_simple_test_report(
            "fly",
            &[
                ("inputs", args.inputs.len().to_string()),
                ("labels", labels.join(",")),
                ("k", args.k.to_string()),
                ("threads", args.threads.to_string()),
                ("output_dir", args.output_dir.clone()),
            ],
            started.elapsed(),
        );
    }
    Ok(())
}
