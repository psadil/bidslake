#!/usr/bin/env bash
# Script to create a bidslake database from 50 OpenNeuro datasets
# This tests multi-dataset support and overall system robustness

set -e # Exit on error

OUTPUT_DB="openneuro_50_datasets.duckdb"
LOG_FILE="ingest_log.txt"

# Remove existing database and log
rm -f "$OUTPUT_DB" "$LOG_FILE"

echo "Starting ingestion of 50 OpenNeuro datasets into $OUTPUT_DB"
echo "Log file: $LOG_FILE"
echo ""

# List of 50 OpenNeuro datasets covering various modalities
datasets=(
	"ds000001" # Balloon Analog Risk-taking Task
	"ds000002" # Classification learning
	"ds000003" # Rhyme judgment
	"ds000005" # Mixed-gambles task
	"ds000006" # Living-nonliving decision
	"ds000007" # Stop-signal task
	"ds000008" # Stop-signal task with manual response
	"ds000009" # The generalization of value
	"ds000011" # Classification learning and tone-counting
	"ds000017" # Classification learning and reversal
	"ds000051" # Cross-language repetition priming
	"ds000052" # Classification learning and stop-signal (1)
	"ds000101" # Simon task dataset
	"ds000102" # Flanker task (event-related)
	"ds000105" # Visual object recognition
	"ds000107" # Word and object processing
	"ds000108" # Prefrontal-striatal-amygdala
	"ds000109" # AOMIC ID1000
	"ds000110" # AOMIC PIOP1
	"ds000113" # Forrest Gump
	"ds000114" # Finger tapping
	"ds000115" # Auditory lexical decision
	"ds000116" # Working memory
	"ds000117" # Multi-echo BOLD
	"ds000120" # Verbal fluency
	"ds000157" # Moral and non-moral decision making
	"ds000164" # Stroop task
	"ds000171" # IBC (Individual Brain Charting)
	"ds000201" # EMBARGOED Sensorimotor learning
	"ds000206" # THP (Test-retest Heterogenity Personality)
	"ds000210" # Divided attention task
	"ds000212" # Reversal learning
	"ds000214" # Semantic classification
	"ds000216" # Articulatory motor control
	"ds000224" # Generalization of fear
	"ds000228" # Reward processing
	"ds000244" # Localizer task
	"ds000247" # Neurofeedback
	"ds000251" # Social cognition
	"ds001246" # Balloon Analog Risk Task (BART)
	"ds001338" # MRI multicenter study
	"ds001378" # MRI SCA2 DTI
	"ds001491" # Mother Of Unification Studies (MOUS)
	"ds001600" # Example Fieldmaps
	"ds001734" # NARPS
	"ds001771" # Face recognition
	"ds002105" # Visual working memory
	"ds002336" # UCLA Consortium for Neuropsychiatric Phenomics
	"ds002578" # N-back task
	"ds002723" # fMRI retest reliability
	"ds003097" # Natural Scenes Dataset (NSD)
)

# S3 support is compiled in unconditionally (no cargo feature gate).
CARGO_CMD="cargo run --release --bin bidslake --"

successful=0
failed=0

for ds in "${datasets[@]}"; do
	echo "=========================================="
	echo "Processing: $ds"
	echo "=========================================="

	start_time=$(date +%s)

	# Run bidslake with S3 input
	if $CARGO_CMD \
		--input "s3://openneuro.org/$ds" \
		--output "$OUTPUT_DB" \
		--dataset-id "$ds" \
		--no-sign-request \
		>>"$LOG_FILE" 2>&1; then

		end_time=$(date +%s)
		duration=$((end_time - start_time))
		echo "✓ Success in ${duration}s"
		((successful++))
	else
		echo "✗ Failed (see $LOG_FILE for details)"
		((failed++))
		# Continue with other datasets even if one fails
	fi

	echo ""
done

echo "=========================================="
echo "Summary"
echo "=========================================="
echo "Total datasets: ${#datasets[@]}"
echo "Successful: $successful"
echo "Failed: $failed"
echo ""
echo "Output database: $OUTPUT_DB"
echo "Log file: $LOG_FILE"

# Show database stats
if [ -f "$OUTPUT_DB" ]; then
	echo ""
	echo "Database statistics:"
	echo "SELECT COUNT(DISTINCT dataset_id) as dataset_count FROM dataset_description;" | duckdb "$OUTPUT_DB"
	echo ""
	echo "SELECT dataset_id, name FROM dataset_description ORDER BY dataset_id;" | duckdb "$OUTPUT_DB"
fi
