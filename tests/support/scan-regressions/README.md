# Scan regression source

Lossless hex copies of the existing `fuzz/regression/fuzz_scan` inputs at
[zenflate 0fa809a9](https://github.com/imazen/zenflate/tree/0fa809a9/fuzz/regression/fuzz_scan).
`tests/fuzz_regression.rs` decodes and replays them through the stable checks.

| File | Decoded bytes | SHA-256 of decoded bytes |
|---|---:|---|
| error-past-decode-buffer.hex | 1537 | `14480a3a2344ffe29485511286e9946a199c4470db2a04452cf53d4228888c95` |
| miniz-rejects-zenflate-accepts.hex | 23 | `8f18882fb34ccf8bb98c02354f0e47ae7c55c0b4b78119ec399de40b70dfe43e` |
