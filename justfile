# Install dwm to ~/.bin and re-sign for macOS
local-install:
    cargo build --release
    cp target/release/dwm ~/.bin/dwm
    codesign --force --sign - ~/.bin/dwm

# Build the website
build-site:
    ./site/build.sh

# Serve the website locally
serve-site:
    npx serv site/dist
