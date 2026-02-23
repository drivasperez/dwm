# Install dwm to ~/.bin and re-sign for macOS
local-install:
    cargo build --release
    cp target/release/dwm ~/.bin/dwm
    codesign --force --sign - ~/.bin/dwm
