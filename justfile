default:
   @just --choose

install_fmt:
    rustup component add rustfmt --toolchain nightly

install_lychee:
    cargo install lychee


integration_test_dir := justfile_directory() / ".scratch/integration_tests"
integration_test_base_url := "https://tycho-test.broxus.cc"

prepare_integration_tests:
    #!/usr/bin/env bash
    # Create the integration test directory if it does not exist
    echo "Integration test directory: {{integration_test_dir}}"
    mkdir -p {{integration_test_dir}}

    # Always download the checksum file first to ensure it's the latest
    echo "Downloading checksum file..."
    curl --request GET -sL --url {{integration_test_base_url}}/states.tar.zst.sha256 --output {{integration_test_dir}}/states.tar.zst.sha256

    # Check if the archive file exists
    if [ -f {{integration_test_dir}}/states.tar.zst ]; then
      # Verify the archive against the checksum
      echo "Verifying existing archive against checksum..."
      cd {{integration_test_dir}}
      if sha256sum -c states.tar.zst.sha256; then
          echo "Checksum matches. No need to download the archive."
      else
          echo "Checksum does not match. Downloading the archive..."
          just _download_archive
      fi
      else
          echo "Archive file does not exist. Downloading the archive..."
          just _download_archive
    fi

_download_archive:
    curl --request GET -L --url {{integration_test_base_url}}/states.tar.zst --output {{integration_test_dir}}/states.tar.zst

clean_integration_tests:
    rm -rf {{integration_test_dir}}


fmt: install_fmt
    cargo +nightly fmt --all

# ci checks
ci: check_dev_docs check_format lint docs test

check_dev_docs:
    lychee {{justfile_directory()}}/docs

check_format: install_fmt
    cargo +nightly fmt --all -- --check

lint:
    cargo clippy --all-targets --all-features --workspace

docs:
    export RUSTDOCFLAGS=-D warnings
    cargo doc --no-deps --document-private-items --all-features --workspace

test:
    cargo test --all-targets --all-features --workspace

# runs all tests including ignored. Will take a lot of time to run
integration_test: prepare_integration_tests
    #cargo test -r --all-targets --all-features --workspace -- --ignored #uncomment this when all crates will compile ˙◠˙
    # for now add tests one by one
    RUST_LIB_BACKTRACE=1 RUST_BACKTRACE=1 cargo test -r --package tycho-storage --lib store::shard_state::store_state_raw::test::insert_and_delete_of_several_shards -- --ignored --exact --nocapture

gen_network n: build_debug
    #!/usr/bin/env bash
    TEMP_DIR="./.temp"
    TYCHO_BIN="./target/debug/tycho"

    mkdir -p "$TEMP_DIR"

    N={{n}}

    GLOBAL_CONFIG='{}'
    NODE_CONFIG=$(cat ./config.json)

    for i in $(seq $N);
    do
        $TYCHO_BIN tool gen-key > "$TEMP_DIR/keys${i}.json"

        PORT=$((20000 + i))

        KEY=$(jq -r .secret < "$TEMP_DIR/keys${i}.json")
        DHT_ENTRY=$($TYCHO_BIN tool gen-dht "127.0.0.1:$PORT" --key "$KEY")

        GLOBAL_CONFIG=$(echo "$GLOBAL_CONFIG" | jq ".bootstrap_peers += [$DHT_ENTRY]")

        NODE_CONFIG=$(echo "$NODE_CONFIG" | jq ".port = $PORT | .storage.root_dir = \"$TEMP_DIR/db${i}\"")
        echo "$NODE_CONFIG" > "$TEMP_DIR/config${i}.json"
    done

    ZEROSTATE=$(cat zerostate.json | jq '.validators = []')
    for i in $(seq $N);
    do
        PUBKEY=$(jq .public < "$TEMP_DIR/keys${i}.json")
        ZEROSTATE=$(echo "$ZEROSTATE" | jq ".validators += [$PUBKEY]")
    done

    echo "$ZEROSTATE" > "$TEMP_DIR/zerostate.json"
    ZEROSTATE_ID=$(
        $TYCHO_BIN tool gen-zerostate "$TEMP_DIR/zerostate.json" \
            --output "$TEMP_DIR/zerostate.boc" \
            --force
    )

    GLOBAL_CONFIG=$(echo "$GLOBAL_CONFIG" | jq ".zerostate = $ZEROSTATE_ID")
    echo "$GLOBAL_CONFIG" > "$TEMP_DIR/global-config.json"

node n: build_debug
    #!/usr/bin/env bash
    TEMP_DIR="./.temp"
    TYCHO_BIN="./target/debug/tycho"

    $TYCHO_BIN node run \
        --keys "$TEMP_DIR/keys{{n}}.json" \
        --config "$TEMP_DIR/config{{n}}.json" \
        --global-config "$TEMP_DIR/global-config.json" \
        --import-zerostate "$TEMP_DIR/zerostate.boc" \
        --logger-config ./logger.json \

init_node_config: build_debug
    #!/usr/bin/env bash
    TYCHO_BIN="./target/debug/tycho"
    $TYCHO_BIN node run --init-config "./config.json"

init_zerostate_config: build_debug
    #!/usr/bin/env bash
    TYCHO_BIN="./target/debug/tycho"
    $TYCHO_BIN tool gen-zerostate --init-config "./zerostate.json"

build_debug:
    cargo build --bin tycho
