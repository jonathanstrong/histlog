export RUSTFLAGS := `[ -x /usr/bin/lld ] && echo "-C link-arg=-fuse-ld=lld" || echo ''`

publish-registry := "crates.io"

cargo +args='':
    cargo {{args}}

check +args='':
    @just cargo check {{args}}

debug-build binary_name +args='':
    @just cargo build --bin {{binary_name}} {{args}}

release-build binary_name +args='':
    @just cargo build --bin {{binary_name}} --release {{args}}

verify-clean-git:
    test "$(echo `git status --porcelain` | wc -c)" -eq "1"

get-crate-version:
    @cat Cargo.toml | toml2json | jq -r '.package.version'

verify-release-tag-does-not-exist:
    VERSION=$(just get-crate-version) \
        && test -z "$(git tag | rg \"v${VERSION}\")" # Error: tag appears to exist already

pre-release:
    just cargo check \
        && just cargo test \
        && just cargo clippy \
        && just cargo check --features smol_str \
        && just cargo test --features smol_str \
        && just cargo clippy --features smol_str \
        && echo "awesome job!"

publish +args='': verify-clean-git verify-release-tag-does-not-exist pre-release
    @just cargo publish --registry {{publish-registry}} {{args}}
    echo "tagging release"
    git tag "v$(just get-crate-version)"
    git push --tags
    rm -rf target/package

example name +args='':
    @just cargo build --example {{name}} {{args}}

test +args='':
    @just cargo test {{args}}

# cargo doc --open
doc +args='':
    @just cargo doc --open {{args}}

# just rebuild docs, don't open browser page again
redoc +args='': 
    @just cargo doc {{args}}

# like doc, but include private items
doc-priv +args='':
    @just cargo doc --open --document-private-items {{args}}

bench +args='':
    @just cargo bench {{args}}

update +args='':
    @just cargo update {{args}}

# blow away build dir and start all over again
rebuild:
    just cargo clean
    just update
    just test

# display env variables that will be used for building
show-build-env:
    @ env | rg RUST --color never


