use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use crate::analyze;
use crate::cli::{AnalyzeArgs, FlyArgs};
use crate::kiteops::{format_count_filter, infer_sample_labels};
use crate::stats::print_simple_test_report;

pub fn run_fly(args: &FlyArgs) -> anyhow::Result<()> {
    let started = Instant::now();
    let stage_total = 3usize;
    eprintln!(
        "[kite fly] workflow start: inputs={}, output_dir={}",
        args.inputs.len(),
        args.output_dir,
    );
    eprintln!(
        "[kite fly] settings: k={}, canonical={}, min={}, max={}, mode={:?}, threads={}, memory={}, partitions={}, chunk_kmers={}, buffer={}MB",
        args.k,
        args.canonical,
        format_count_filter(args.min),
        format_count_filter(args.max),
        args.mode,
        args.threads,
        args.memory.as_deref().unwrap_or("auto"),
        args.partitions,
        args.chunk_kmers,
        args.buffer,
    );
    eprintln!(
        "[kite fly] analysis: correlation={:?}, embedding={:?}, heatmap_kmeans={}, heatmap_seed={}, cluster_samples={}",
        args.correlation,
        args.embedding,
        args.heatmap_kmeans,
        args.heatmap_seed,
        args.cluster_samples,
    );
    eprintln!("[kite fly] stage 0/{stage_total}: validating inputs and output directories");
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
    let workflow_report_path = Path::new(&args.output_dir).join("kite_flye_workflow_report.md");
    eprintln!(
        "[kite fly] labels={} ({})",
        labels.join(","),
        if args.labels.is_some() {
            "user supplied"
        } else {
            "inferred from file names"
        }
    );
    for (index, (input, label)) in args.inputs.iter().zip(labels.iter()).enumerate() {
        eprintln!(
            "[kite fly]   input #{:>2}: label={} path={}",
            index + 1,
            label,
            input
        );
    }
    eprintln!(
        "[kite fly] stage 0/{stage_total}: ready (databases={}, analysis={})",
        db_dir.display(),
        analysis_dir.display()
    );

    eprintln!(
        "[kite fly] stage 1/{stage_total}: counting {} input file(s)",
        args.inputs.len(),
    );
    let mut kite_inputs = Vec::with_capacity(args.inputs.len());
    let stage1_started = Instant::now();
    let count_plan = derive_count_plan(args, args.inputs.len())?;
    eprintln!(
        "[kite fly] stage 1/{stage_total}: plan jobs={} parallel={} threads/job={} memory/job={}",
        args.inputs.len(),
        count_plan.parallel_jobs,
        count_plan.threads_per_job,
        count_plan
            .memory_per_job
            .clone()
            .unwrap_or_else(|| "auto".to_string())
    );
    let count_jobs = args
        .inputs
        .iter()
        .enumerate()
        .map(|(index, input)| CountJob {
            index,
            label: labels[index].clone(),
            input: input.clone(),
            output: db_dir.join(format!("{}.kite", labels[index])),
        })
        .collect::<Vec<_>>();
    let count_records = run_count_stage(args, &count_jobs, &count_plan)?;
    let stage1_seconds = stage1_started.elapsed().as_secs_f64();
    eprintln!(
        "[kite fly] stage 1/{stage_total}: complete in {:.3}s (sum of per-input times {:.3}s)",
        stage1_seconds,
        count_records.iter().map(|record| record.seconds).sum::<f64>()
    );
    for record in &count_records {
        kite_inputs.push(record.output.to_string_lossy().to_string());
    }

    eprintln!(
        "[kite fly] stage 2/{stage_total}: analyzing {} .kite database(s)",
        kite_inputs.len()
    );
    let analyze_started = Instant::now();
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
        threads: args.threads,
        progress: args.progress,
        test_mode: false,
        _help: None,
    };
    analyze::run_analyze(&analyze_args)?;
    let analyze_seconds = analyze_started.elapsed().as_secs_f64();
    eprintln!(
        "[kite fly] stage 2/{stage_total}: analyze complete in {:.3}s",
        analyze_seconds
    );
    let intermediates_kept = if args.keep_intermediate {
        true
    } else {
        eprintln!(
            "[kite fly] stage 3/{stage_total}: removing intermediate databases because --keep-intermediate=false"
        );
        std::fs::remove_dir_all(&db_dir)?;
        false
    };
    write_workflow_report(
        &workflow_report_path,
        args,
        &labels,
        &count_records,
        &count_plan,
        stage1_seconds,
        &analysis_dir,
        analyze_seconds,
        intermediates_kept,
        started.elapsed().as_secs_f64(),
    )?;
    eprintln!("[kite fly] stage 3/{stage_total}: final output summary");
    eprintln!("[kite fly]   databases: {}", db_dir.display());
    eprintln!("[kite fly]   analysis:  {}", analysis_dir.display());
    eprintln!(
        "[kite fly]   report:    {}",
        analysis_dir.join("analysis_report.qmd").display()
    );
    eprintln!(
        "[kite fly]   workflow report: {}",
        workflow_report_path.display()
    );
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
                ("keep_intermediate", args.keep_intermediate.to_string()),
                (
                    "correlation",
                    format!("{:?}", args.correlation).to_lowercase(),
                ),
                ("embedding", format!("{:?}", args.embedding).to_lowercase()),
                ("output_dir", args.output_dir.clone()),
                (
                    "workflow_report",
                    workflow_report_path.display().to_string(),
                ),
            ],
            started.elapsed(),
        );
    }
    eprintln!("[kite fly] done in {:.3}s", started.elapsed().as_secs_f64());
    Ok(())
}

struct CountStageRecord {
    index: usize,
    label: String,
    input: String,
    output: PathBuf,
    output_size: u64,
    seconds: f64,
    threads: usize,
    memory: Option<String>,
}

#[derive(Clone)]
struct CountJob {
    index: usize,
    label: String,
    input: String,
    output: PathBuf,
}

struct CountPlan {
    parallel_jobs: usize,
    threads_per_job: usize,
    memory_per_job: Option<String>,
}

struct CountTaskResult {
    record: CountStageRecord,
    stderr: String,
    stdout: String,
}

#[derive(Clone)]
struct CountTaskConfig {
    k: u8,
    canonical: bool,
    min: Option<u32>,
    max: Option<u32>,
    mode: String,
    partitions: usize,
    chunk_kmers: usize,
    buffer: usize,
    progress: bool,
}

fn derive_count_plan(args: &FlyArgs, _job_count: usize) -> anyhow::Result<CountPlan> {
    let threads_per_job = if args.threads == 0 {
        num_cpus::get().max(1)
    } else {
        args.threads.max(1)
    };
    Ok(CountPlan {
        parallel_jobs: 1,
        threads_per_job,
        memory_per_job: args.memory.clone(),
    })
}

fn run_count_stage(
    args: &FlyArgs,
    jobs: &[CountJob],
    plan: &CountPlan,
) -> anyhow::Result<Vec<CountStageRecord>> {
    let task_config = CountTaskConfig {
        k: args.k,
        canonical: args.canonical,
        min: args.min,
        max: args.max,
        mode: format!("{:?}", args.mode).to_lowercase(),
        partitions: args.partitions,
        chunk_kmers: args.chunk_kmers,
        buffer: args.buffer,
        progress: args.progress,
    };
    let mut completed = Vec::with_capacity(jobs.len());
    for job in jobs {
        eprintln!(
            "[kite fly] stage 1/3: launch count {}/{} label={} input={} output={} threads={} memory={}",
            job.index + 1,
            jobs.len(),
            job.label,
            job.input,
            job.output.display(),
            plan.threads_per_job,
            plan.memory_per_job.as_deref().unwrap_or("auto")
        );
        let result = run_count_task(
            &task_config,
            job,
            plan.threads_per_job,
            plan.memory_per_job.as_deref(),
        )?;
        eprintln!(
            "[kite fly] stage 1/3: count {}/{} complete label={} wall={:.3}s bytes={} threads={} memory={}",
            result.record.index + 1,
            jobs.len(),
            result.record.label,
            result.record.seconds,
            result.record.output_size,
            result.record.threads,
            result.record.memory.clone().unwrap_or_else(|| "auto".to_string())
        );
        if !result.stdout.is_empty() {
            eprint!("{}", result.stdout);
        }
        if !result.stderr.is_empty() {
            eprint!("{}", result.stderr);
        }
        completed.push(result.record);
    }
    completed.sort_by_key(|record| record.index);
    Ok(completed)
}

fn run_count_task(
    config: &CountTaskConfig,
    job: &CountJob,
    threads: usize,
    memory: Option<&str>,
) -> anyhow::Result<CountTaskResult> {
    let started = Instant::now();
    let exe = std::env::current_exe()?;
    let mut command = Command::new(exe);
    command
        .arg("count")
        .arg("-i")
        .arg(&job.input)
        .arg("-o")
        .arg(&job.output)
        .arg("-k")
        .arg(config.k.to_string())
        .arg("--canonical")
        .arg(config.canonical.to_string())
        .arg("--mode")
        .arg(&config.mode)
        .arg("--threads")
        .arg(threads.to_string())
        .arg("--partitions")
        .arg(config.partitions.to_string())
        .arg("--chunk-kmers")
        .arg(config.chunk_kmers.to_string())
        .arg("--buffer")
        .arg(config.buffer.to_string());
    if let Some(memory) = memory {
        command.arg("--memory").arg(memory);
    }
    if let Some(min) = config.min {
        command.arg("--min").arg(min.to_string());
    }
    if let Some(max) = config.max {
        command.arg("--max").arg(max.to_string());
    }
    if config.progress {
        command.arg("-p");
    }
    let output = command.output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        anyhow::bail!(
            "count stage failed for input {} with status {}\n{}{}",
            job.input,
            output.status,
            stdout,
            stderr
        );
    }
    let output_size = std::fs::metadata(&job.output)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    Ok(CountTaskResult {
        record: CountStageRecord {
            index: job.index,
            label: job.label.clone(),
            input: job.input.clone(),
            output: job.output.clone(),
            output_size,
            seconds: started.elapsed().as_secs_f64(),
            threads,
            memory: memory.map(str::to_string),
        },
        stderr,
        stdout,
    })
}

fn write_workflow_report(
    path: &Path,
    args: &FlyArgs,
    labels: &[String],
    count_records: &[CountStageRecord],
    count_plan: &CountPlan,
    count_stage_seconds: f64,
    analysis_dir: &Path,
    analyze_seconds: f64,
    intermediates_kept: bool,
    total_seconds: f64,
) -> anyhow::Result<()> {
    let mut out = BufWriter::new(std::fs::File::create(path)?);
    writeln!(out, "# Kite Fly Workflow Report")?;
    writeln!(out)?;
    writeln!(out, "## Summary")?;
    writeln!(out)?;
    writeln!(out, "| Field | Value |")?;
    writeln!(out, "|---|---:|")?;
    writeln!(out, "| Inputs | {} |", args.inputs.len())?;
    writeln!(out, "| k | {} |", args.k)?;
    writeln!(out, "| Canonical | {} |", args.canonical)?;
    writeln!(out, "| Min | {} |", format_count_filter(args.min))?;
    writeln!(out, "| Max | {} |", format_count_filter(args.max))?;
    writeln!(out, "| Count mode | {:?} |", args.mode)?;
    writeln!(out, "| Threads | {} |", args.threads)?;
    writeln!(
        out,
        "| Memory | {} |",
        args.memory.as_deref().unwrap_or("auto")
    )?;
    writeln!(out, "| Keep intermediate | {} |", intermediates_kept)?;
    writeln!(out, "| Total seconds | {:.3} |", total_seconds)?;
    writeln!(out)?;
    writeln!(out, "## Labels")?;
    writeln!(out)?;
    writeln!(out, "{}", labels.join(", "))?;
    writeln!(out)?;
    writeln!(out, "## Stage 1: Count")?;
    writeln!(out)?;
    writeln!(out, "- Stage wall time: {:.3}s", count_stage_seconds)?;
    writeln!(out, "- Parallel jobs: {}", count_plan.parallel_jobs)?;
    writeln!(out, "- Threads per job: {}", count_plan.threads_per_job)?;
    writeln!(
        out,
        "- Memory per job: {}",
        count_plan
            .memory_per_job
            .clone()
            .unwrap_or_else(|| "auto".to_string())
    )?;
    writeln!(
        out,
        "- Sum of per-input wall times: {:.3}s",
        count_records.iter().map(|record| record.seconds).sum::<f64>()
    )?;
    writeln!(out)?;
    writeln!(
        out,
        "| # | Label | Input | Output | Output bytes | Seconds | Threads | Memory |"
    )?;
    writeln!(out, "|---:|---|---|---|---:|---:|---:|---|")?;
    for (index, record) in count_records.iter().enumerate() {
        writeln!(
            out,
            "| {} | {} | `{}` | `{}` | {} | {:.3} | {} | {} |",
            index + 1,
            record.label,
            record.input,
            record.output.display(),
            record.output_size,
            record.seconds,
            record.threads,
            record.memory.as_deref().unwrap_or("auto")
        )?;
    }
    writeln!(out)?;
    writeln!(out, "## Stage 2: Analyze")?;
    writeln!(out)?;
    writeln!(out, "- Output directory: `{}`", analysis_dir.display())?;
    writeln!(out, "- Seconds: {:.3}", analyze_seconds)?;
    writeln!(out, "- Correlation: {:?}", args.correlation)?;
    writeln!(out, "- Embedding: {:?}", args.embedding)?;
    writeln!(out, "- Heatmap clusters: {}", args.heatmap_kmeans)?;
    writeln!(out, "- Heatmap seed: {}", args.heatmap_seed)?;
    writeln!(out, "- Cluster samples: {}", args.cluster_samples)?;
    writeln!(out)?;
    writeln!(out, "## Stage 3: Outputs")?;
    writeln!(out)?;
    writeln!(
        out,
        "- Analysis report: `{}`",
        analysis_dir.join("analysis_report.md").display()
    )?;
    writeln!(
        out,
        "- Quarto report: `{}`",
        analysis_dir.join("analysis_report.qmd").display()
    )?;
    writeln!(out, "- Intermediate databases kept: {}", intermediates_kept)?;
    out.flush()?;
    Ok(())
}
