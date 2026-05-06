## 1. kmc count+dump

### 1.1 k21
#### command: /usr/bin/time -v sh -c "kmc -k21 -t48 -m48 -ci1 -cs1000000000 -fq test.fastq.gz test_k21.kmc k21_tmp_kmc"

#### /usr/bin/time -v sh -c "kmc_dump test_k21.kmc k21.kmc.tsv"

### 1.2 k31

#### command: /usr/bin/time -v sh -c "kmc -k31 -t48 -m48 -ci1 -cs1000000000 -fq test.fastq.gz test_k31.kmc k31_tmp_kmc"

#### /usr/bin/time -v sh -c "kmc_dump test_k31.kmc k31.kmc.tsv"


### 1.3 k51 

#### command: /usr/bin/time -v sh -c "kmc -k51 -t48 -m48 -ci1 -cs1000000000 -fq test.fastq.gz test_k51.kmc k51_tmp_kmc"

#### /usr/bin/time -v sh -c "kmc_dump test_k51.kmc k51.kmc.tsv"

## 2. kite count+dump

### 1.1 k21

#### command: /usr/bin/time -v sh -c "/eight_TB/BigMQ/Users/Hua/kmer_counter/kite/target/release/kite count -k 21 --threads 48 --max-memory-gb 48 -i test.fastq.gz -o test_k21.kite --test-mode"

#### /usr/bin/time -v sh -c "/eight_TB/BigMQ/Users/Hua/kmer_counter/kite/target/release/kite dump --threads 48 -i test_k21.kite -o test_k21_kite.tsv --test-mode"
