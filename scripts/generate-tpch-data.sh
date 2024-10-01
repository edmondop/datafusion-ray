#!/bin/bash

set -e
if [ -z "$TPCH_SAMPLING_RATE" ]; then
	echo "Error: TPCH_SAMPLING_RATE is not defined."
	exit 1
fi

if [ -z "$TPCH_DATA_PATH" ]; then
	echo "Error: TPCH_DATA_PATH is not defined."
	exit 1
fi

if [ ! -d "$TPCH_DATA_PATH" ]; then
	echo "Creating TPCH data for testing..."
	git clone https://github.com/databricks/tpch-dbgen.git
	cd tpch-dbgen
	make
	./dbgen -f -s "$TPCH_SAMPLING_RATE"
	mkdir -p ../"$TPCH_DATA_PATH"
	mv ./*.tbl ../"$TPCH_DATA_PATH"
	cd .. && rm -rf tpch-dbgen
else
	echo "TPCH data already exists. Skipping clone and generation."
fi
