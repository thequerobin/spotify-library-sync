$ErrorActionPreference = 'Stop'
Push-Location (Join-Path $PSScriptRoot '..')
try {
    cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    New-Item -ItemType Directory -Force -Path dist/spotify-library-sync | Out-Null
    Copy-Item target/release/spotify-sync.exe, README.md, LICENSE, .env.example, packaging/abrir-terminal.cmd dist/spotify-library-sync/
    Compress-Archive -Path dist/spotify-library-sync -DestinationPath dist/spotify-library-sync-windows-x64.zip -Force
} finally { Pop-Location }
