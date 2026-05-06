/// Lightweight FASTQ parser using a byte-level state machine.
/// Only extracts sequence lines — skips read name, '+' line, and quality.
/// Zero-copy: yields byte slices into the input buffer.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Header, // '@' line
    Seq,    // sequence line
    Plus,   // '+' line
    Qual,   // quality line
}

/// Parse all sequence lines from a FASTQ byte buffer.
/// Calls `on_seq` for each sequence slice found. This is the hot path.
///
/// # Errors
/// Returns an error string if the input is malformed (e.g., does not start with '@').
#[inline]
pub fn parse_sequences<F>(data: &[u8], mut on_seq: F) -> Result<(), &'static str>
where
    F: FnMut(&[u8]),
{
    if data.is_empty() {
        return Ok(());
    }
    // First byte must be '@'
    if data[0] != b'@' {
        return Err("FASTQ chunk does not start with '@'");
    }

    let mut state = State::Header;
    let mut seq_start = 0usize;
    let mut i = 0usize;
    let len = data.len();

    while i < len {
        let b = data[i];
        if b == b'\n' {
            match state {
                State::Header => {
                    state = State::Seq;
                    seq_start = i + 1;
                }
                State::Seq => {
                    let seq = &data[seq_start..i];
                    if !seq.is_empty() {
                        on_seq(seq);
                    }
                    state = State::Plus;
                }
                State::Plus => {
                    state = State::Qual;
                }
                State::Qual => {
                    state = State::Header;
                }
            }
        }
        i += 1;
    }
    // Handle incomplete last record (no trailing newline) — emit partial seq if needed
    // In our aligned-chunk design, the last record should always end with a newline,
    // but be safe: if we're mid-Seq, emit it.
    if state == State::Seq && seq_start < len {
        on_seq(&data[seq_start..len]);
    }
    Ok(())
}

/// Count records and bases seen in a FASTQ chunk.
#[allow(dead_code)]
pub fn count_records_and_bases(data: &[u8]) -> (u64, u64) {
    let mut reads = 0u64;
    let mut bases = 0u64;
    let _ = parse_sequences(data, |seq| {
        reads += 1;
        bases += seq.len() as u64;
    });
    (reads, bases)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic() {
        let input = b"@READ1\nACGT\n+\nIIII\n@READ2\nTTTT\n+\nIIII\n";
        let mut seqs: Vec<Vec<u8>> = Vec::new();
        parse_sequences(input, |s| seqs.push(s.to_vec())).unwrap();
        assert_eq!(seqs, vec![b"ACGT".to_vec(), b"TTTT".to_vec()]);
    }

    #[test]
    fn test_parse_with_n() {
        let input = b"@R\nACNGT\n+\nIIIII\n";
        let mut seqs: Vec<Vec<u8>> = Vec::new();
        parse_sequences(input, |s| seqs.push(s.to_vec())).unwrap();
        assert_eq!(seqs, vec![b"ACNGT".to_vec()]);
    }

    #[test]
    fn test_parse_empty() {
        let mut count = 0;
        parse_sequences(b"", |_| count += 1).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_parse_bad_start() {
        let result = parse_sequences(b"NOTFASTQ", |_| {});
        assert!(result.is_err());
    }

    #[test]
    fn test_count_records() {
        let input = b"@R1\nACGT\n+\nIIII\n@R2\nGGCC\n+\nIIII\n";
        let (reads, bases) = count_records_and_bases(input);
        assert_eq!(reads, 2);
        assert_eq!(bases, 8);
    }
}
