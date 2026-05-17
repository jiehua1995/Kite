use std::path::Path;
use std::time::Instant;

use crate::analyze;
use crate::cli::{AnalyzeArgs, CountArgs, FlyArgs};
use crate::count;
use crate::kiteops::{format_count_filter, infer_sample_labels};
use crate::stats::print_simple_test_report;

pub fn run_fly(args: &FlyArgs) -> anyhow::Result<()> {
    let started = Instant::now();
    eprintln!(
        "[kite fly] inputs={}, output_dir={}, k={}, canonical={}, min={}, max={}, mode={:?}, threads={}",
        args.inputs.len(),
        args.output_dir,
        args.k,
        args.canonical,
        format_count_filter(args.min),
        format_count_filter(args.max),
        args.mode,
        args.threads,
    );
    if let (Some(min), Some(max)) = (args.min, args.max) {
        if min > max {
            anyhow::bail!("--min ({}) must be <= --max ({})", min, max);
        }
    }
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
            partitions: args.partitions,
            chunk_kmers: args.chunk_kmers,
            memory: args.memory.clone(),
            canonical: args.canonical,
            min: args.min,
            max: args.max,
            progress: args.progress,
            test_mode: false,
            mode: args.mode,
            buffer: args.buffer,
            _help: None,
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
        _help: None,
    };
    analyze::run_analyze(&analyze_args)?;
    if args.test_mode {
        print_simple_test_report(
            "fly",
            &[
                ("inputs", args.inputs.len().to_string()),
                ("labels", labels.join(",")),
                ("k", args.k.to_string()),
                ("canonical", args.canonical.to_string()),
                ("min", format_count_filter(args.min)),
                ("max", format_count_filter(args.max)),
                ("count_mode", format!("{:?}", args.mode).to_lowercase()),
                ("threads", args.threads.to_string()),
                ("partitions", args.partitions.to_string()),
                ("chunk_kmers", args.chunk_kmers.to_string()),
                ("buffer_mb", args.buffer.to_string()),
                (
                    "correlation",
                    format!("{:?}", args.correlation).to_lowercase(),
                ),
                ("embedding", format!("{:?}", args.embedding).to_lowercase()),
                ("output_dir", args.output_dir.clone()),
            ],
            started.elapsed(),
        );
    }
    eprintln!("[kite fly] done in {:.3}s", started.elapsed().as_secs_f64());
    Ok(())
}
