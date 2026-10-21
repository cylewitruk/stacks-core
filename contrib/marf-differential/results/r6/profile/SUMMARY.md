## Timed-loop totals (collection restricted to the phase's timed-pass function)

| profile | phase | lookups (n x passes) | Ir, timed loop | Ir per lookup |
|---|---|---|---|---|
| lateprobe-4096-patchednodes1024-warm | warm | 8,192 x 5 | 1,389,843,754 | 33,932 |
| lateprobe-4096-patchednodes64-warm | warm | 8,192 x 5 | 1,046,390,176 | 25,547 |
| lateprobe-4096-patchfree-warm | warm | 2,000 x 5 | 351,636,927 | 35,164 |
| lateprobe-64-patchednodes1024-warm | warm | 8,192 x 5 | 1,638,175,353 | 39,995 |
| lateprobe-64-patchednodes64-warm | warm | 8,192 x 5 | 1,046,586,031 | 25,551 |
| lateprobe-64-patchfree-warm | warm | 2,000 x 5 | 351,615,698 | 35,162 |
| reference-4096-patchednodes1024-warm | warm | 8,192 x 5 | 2,425,792,569 | 59,223 |
| reference-4096-patchednodes64-cold | cold | 8,192 x 1 | 223,416,650 | 27,273 |
| reference-4096-patchednodes64-warm | warm | 8,192 x 5 | 1,103,998,843 | 26,953 |
| reference-4096-patchfree-warm | warm | 2,000 x 5 | 353,485,510 | 35,349 |
| reference-64-patchednodes1024-cold | cold | 8,192 x 1 | 337,849,038 | 41,241 |
| reference-64-patchednodes1024-warm | warm | 8,192 x 5 | 1,687,168,282 | 41,191 |
| reference-64-patchednodes64-cold | cold | 8,192 x 1 | 222,142,174 | 27,117 |
| reference-64-patchednodes64-warm | warm | 8,192 x 5 | 1,104,132,394 | 26,956 |
| reference-64-patchfree-warm | warm | 2,000 x 5 | 353,486,228 | 35,349 |
| rewrite-4096-patchednodes1024-cold | cold | 8,192 x 1 | 89,463,156 | 10,921 |
| rewrite-4096-patchednodes1024-warm | warm | 8,192 x 5 | 406,776,630 | 9,931 |
| rewrite-4096-patchednodes64-cold | cold | 8,192 x 1 | 96,792,586 | 11,816 |
| rewrite-4096-patchednodes64-warm | warm | 8,192 x 5 | 475,232,570 | 11,602 |
| rewrite-4096-patchfree-warm | warm | 2,000 x 5 | 151,306,585 | 15,131 |
| rewrite-64-patchednodes1024-warm | warm | 8,192 x 5 | 1,193,479,949 | 29,138 |
| rewrite-64-patchednodes64-cold | cold | 8,192 x 1 | 99,760,400 | 12,178 |
| rewrite-64-patchednodes64-warm | warm | 8,192 x 5 | 490,062,143 | 11,964 |
| rewrite-64-patchfree-warm | warm | 2,000 x 5 | 151,448,615 | 15,145 |

## Inclusive cost of selected functions inside the timed loop (share of the same total; nested costs overlap and are not additive)

| profile | SharedLru::get | SharedLru::put_if_current | read_patched_persisted_node | read_node_with_state | TrieCursor::walk_read | TrieNode256::load_from_parts | ptrs_from_parts_into | TriePtr::from_slice_compressed | ReadTrieNode::decoded_from_bytes | NodeRecord::decode_node | MarfReadState::apply_patches | get_block_identifier (SQLite) | get_block_id_caching_impl | rusqlite Rows::advance | sqlite3_step | memcpy | rewrite resolve_in | rewrite BranchView::skip_scan | rewrite BranchView::fill_with_end | rewrite apply_patch | SipHash (DefaultHasher::write) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| lateprobe-4096-patchednodes1024-warm | 502,946,060 (36.2%) | - | 4,597,144 (0.3%) | 625,549,464 (45.0%) | 194,802,625 (14.0%) | 82,693,755 (5.9%) | 86,041,580 (6.2%) | 41,322,610 (3.0%) | 138,530,380 (10.0%) | 110,234,640 (7.9%) | - | 260,177,920 (18.7%) | 288,153,600 (20.7%) | 159,744,000 (11.5%) | 155,934,720 (11.2%) | 415,281,595 (29.9%) | - | - | - | - | - |
| lateprobe-4096-patchednodes64-warm | 23,097,520 (2.2%) | - | 11,628,523 (1.1%) | 148,737,438 (14.2%) | 326,126,238 (31.2%) | 198,108,055 (18.9%) | 200,940,530 (19.2%) | 97,632,055 (9.3%) | 270,341,178 (25.8%) | 236,518,433 (22.6%) | - | 260,177,920 (24.9%) | 288,153,600 (27.5%) | 159,744,000 (15.3%) | 155,934,720 (14.9%) | 88,101,555 (8.4%) | - | - | - | - | - |
| lateprobe-4096-patchfree-warm | - | - | - | 20,749,510 (5.9%) | 193,927,319 (55.1%) | 139,610,000 (39.7%) | 146,499,335 (41.7%) | 97,939,370 (27.9%) | 180,657,909 (51.4%) | 166,997,119 (47.5%) | - | 63,520,313 (18.1%) | 70,350,313 (20.0%) | 39,000,000 (11.1%) | 38,070,000 (10.8%) | 25,089,180 (7.1%) | - | - | - | - | - |
| lateprobe-64-patchednodes1024-warm | 47,515,260 (2.9%) | 55,897,184 (3.4%) | 616,460,577 (37.6%) | 874,973,718 (53.4%) | 193,979,220 (11.8%) | 315,230,235 (19.2%) | 317,901,785 (19.4%) | 153,577,990 (9.4%) | 137,706,975 (8.4%) | 108,658,355 (6.6%) | 251,686,375 (15.4%) | 260,177,920 (15.9%) | 288,153,600 (17.6%) | 159,744,000 (9.8%) | 155,934,720 (9.5%) | 128,772,037 (7.9%) | - | - | - | - | - |
| lateprobe-64-patchednodes64-warm | 23,128,660 (2.2%) | - | 11,628,523 (1.1%) | 148,767,333 (14.2%) | 326,292,198 (31.2%) | 198,108,055 (18.9%) | 200,940,530 (19.2%) | 97,632,055 (9.3%) | 270,507,138 (25.8%) | 235,784,453 (22.5%) | - | 260,177,920 (24.9%) | 288,153,600 (27.5%) | 159,744,000 (15.3%) | 155,934,720 (14.9%) | 89,032,635 (8.5%) | - | - | - | - | - |
| lateprobe-64-patchfree-warm | - | - | - | 20,749,510 (5.9%) | 193,906,060 (55.1%) | 139,610,000 (39.7%) | 146,499,335 (41.7%) | 97,939,370 (27.9%) | 180,656,640 (51.4%) | 166,995,850 (47.5%) | - | 63,520,343 (18.1%) | 70,350,343 (20.0%) | 39,000,000 (11.1%) | 38,070,000 (10.8%) | 25,069,190 (7.1%) | - | - | - | - | - |
| reference-4096-patchednodes1024-warm | 1,573,718,230 (64.9%) | - | 4,597,144 (0.2%) | 1,661,410,659 (68.5%) | 194,841,075 (8.0%) | 82,693,755 (3.4%) | 86,041,580 (3.5%) | 41,322,610 (1.7%) | 138,568,830 (5.7%) | 110,273,090 (4.5%) | - | 260,177,920 (10.7%) | 288,153,600 (11.9%) | 159,744,000 (6.6%) | 155,934,720 (6.4%) | 403,243,445 (16.6%) | - | - | - | - | - |
| reference-4096-patchednodes64-cold | 21,766,854 (9.7%) | 53,407 (0.0%) | 3,756,770 (1.7%) | 43,097,907 (19.3%) | 65,417,166 (29.3%) | 40,167,186 (18.0%) | 40,730,831 (18.2%) | 19,792,510 (8.9%) | 54,266,170 (24.3%) | 47,321,633 (21.2%) | - | 52,066,272 (23.3%) | 57,972,444 (25.9%) | 32,102,322 (14.4%) | 31,339,066 (14.0%) | 15,953,718 (7.1%) | - | - | - | - | - |
| reference-4096-patchednodes64-warm | 109,300,240 (9.9%) | - | 11,628,523 (1.1%) | 207,264,163 (18.8%) | 325,117,320 (29.4%) | 198,108,055 (17.9%) | 200,940,530 (18.2%) | 97,632,055 (8.8%) | 269,332,280 (24.4%) | 235,509,535 (21.3%) | - | 260,177,920 (23.6%) | 288,153,600 (26.1%) | 159,744,000 (14.5%) | 155,934,720 (14.1%) | 78,489,195 (7.1%) | - | - | - | - | - |
| reference-4096-patchfree-warm | 1,726,530 (0.5%) | - | - | 22,596,910 (6.4%) | 193,928,524 (54.9%) | 139,610,000 (39.5%) | 146,499,335 (41.4%) | 97,939,370 (27.7%) | 180,659,114 (51.1%) | 166,998,324 (47.2%) | - | 63,520,291 (18.0%) | 70,350,291 (19.9%) | 39,000,000 (11.0%) | 38,070,000 (10.8%) | 25,089,180 (7.1%) | - | - | - | - | - |
| reference-64-patchednodes1024-cold | 23,789,316 (7.0%) | 11,114,704 (3.3%) | 123,572,230 (36.6%) | 185,117,907 (54.8%) | 38,605,501 (11.4%) | 63,135,472 (18.7%) | 63,669,008 (18.8%) | 30,761,780 (9.1%) | 27,351,357 (8.1%) | 21,692,209 (6.4%) | 50,412,011 (14.9%) | 52,066,010 (15.4%) | 57,661,646 (17.1%) | 32,102,322 (9.5%) | 31,339,066 (9.3%) | 24,075,145 (7.1%) | - | - | - | - | - |
| reference-64-patchednodes1024-warm | 119,391,170 (7.1%) | 55,898,454 (3.3%) | 616,456,117 (36.5%) | 924,702,487 (54.8%) | 193,242,519 (11.5%) | 315,230,235 (18.7%) | 317,901,785 (18.8%) | 153,577,990 (9.1%) | 136,970,274 (8.1%) | 108,674,534 (6.4%) | 251,686,375 (14.9%) | 260,177,920 (15.4%) | 288,153,600 (17.1%) | 159,744,000 (9.5%) | 155,934,720 (9.2%) | 120,397,901 (7.1%) | - | - | - | - | - |
| reference-64-patchednodes64-cold | 21,766,854 (9.8%) | 53,988 (0.0%) | 3,756,996 (1.7%) | 43,103,683 (19.4%) | 64,391,376 (29.0%) | 40,167,186 (18.1%) | 40,730,831 (18.3%) | 19,792,510 (8.9%) | 53,233,057 (24.0%) | 46,468,508 (20.9%) | - | 52,066,073 (23.4%) | 57,817,148 (26.0%) | 32,102,322 (14.5%) | 31,339,066 (14.1%) | 15,780,880 (7.1%) | - | - | - | - | - |
| reference-64-patchednodes64-warm | 109,300,240 (9.9%) | - | 11,628,523 (1.1%) | 207,213,403 (18.8%) | 325,392,491 (29.5%) | 198,108,055 (17.9%) | 200,940,530 (18.2%) | 97,632,055 (8.8%) | 269,607,431 (24.4%) | 235,784,686 (21.4%) | - | 260,177,920 (23.6%) | 288,153,600 (26.1%) | 159,744,000 (14.5%) | 155,934,720 (14.1%) | 78,489,215 (7.1%) | - | - | - | - | - |
| reference-64-patchfree-warm | 1,726,530 (0.5%) | - | - | 22,596,910 (6.4%) | 193,929,189 (54.9%) | 139,610,000 (39.5%) | 146,499,335 (41.4%) | 97,939,370 (27.7%) | 180,659,779 (51.1%) | 166,998,989 (47.2%) | - | 63,520,344 (18.0%) | 70,350,344 (19.9%) | 39,000,000 (11.0%) | 38,070,000 (10.8%) | 25,089,180 (7.1%) | - | - | - | - | - |
| rewrite-4096-patchednodes1024-cold | - | - | - | - | - | - | - | - | - | - | - | - | - | - | - | 245,760 (0.3%) | 29,444,238 (32.9%) | 9,492,065 (10.6%) | 3,248,856 (3.6%) | 1,857,857 (2.1%) | 4,088,656 (4.6%) |
| rewrite-4096-patchednodes1024-warm | - | - | - | - | - | - | - | - | - | - | - | - | - | - | - | 1,228,800 (0.3%) | 106,873,050 (26.3%) | 47,460,325 (11.7%) | - | - | 19,023,625 (4.7%) |
| rewrite-4096-patchednodes64-cold | - | - | - | - | - | - | - | - | - | - | - | - | - | - | - | 245,760 (0.3%) | 24,067,190 (24.9%) | 22,217,057 (23.0%) | 837,394 (0.9%) | 457,184 (0.5%) | 3,658,567 (3.8%) |
| rewrite-4096-patchednodes64-warm | - | - | - | - | - | - | - | - | - | - | - | - | - | - | - | 1,228,800 (0.3%) | 111,799,710 (23.5%) | 111,085,285 (23.4%) | - | - | 18,192,470 (3.8%) |
| rewrite-4096-patchfree-warm | - | - | - | - | - | - | - | - | - | - | - | - | - | - | - | 300,000 (0.2%) | 24,262,290 (16.0%) | 66,124,125 (43.7%) | - | - | - |
| rewrite-64-patchednodes1024-warm | - | - | - | - | - | - | - | - | - | - | - | - | - | - | - | 31,010,886 (2.6%) | 893,576,369 (74.9%) | 47,460,325 (4.0%) | 433,572,710 (36.3%) | 247,571,889 (20.7%) | 24,934,801 (2.1%) |
| rewrite-64-patchednodes64-cold | - | - | - | - | - | - | - | - | - | - | - | - | - | - | - | 245,760 (0.2%) | 27,034,513 (27.1%) | 22,217,057 (22.3%) | 2,519,312 (2.5%) | 1,370,066 (1.4%) | 3,679,578 (3.7%) |
| rewrite-64-patchednodes64-warm | - | - | - | - | - | - | - | - | - | - | - | - | - | - | - | 1,228,800 (0.3%) | 126,629,283 (25.8%) | 111,085,285 (22.7%) | 8,419,807 (1.7%) | 4,571,586 (0.9%) | 18,301,900 (3.7%) |
| rewrite-64-patchfree-warm | - | - | - | - | - | - | - | - | - | - | - | - | - | - | - | 300,000 (0.2%) | 24,262,290 (16.0%) | 66,124,125 (43.7%) | - | - | - |

## Top self (exclusive) costs inside the timed loop

**lateprobe-4096-patchednodes1024-warm**

| Ir (self) | share | function (file) |
|---|---|---|
| 415,281,595 | 29.9% | `./string/../sysdeps/x86_64/multiarch/memmove-vec-unaligned-erms.S:__memcpy_avx_unaligned_erms` |
| 82,656,900 | 6.0% | `core/src/slice/cmp.rs:<stacks_marf::cache::SharedLru<(stacks_common::types::chainstate::StacksBlockId, u64), (` |
| 69,263,360 | 5.0% | `sha2-0.10.9/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 57,184,685 | 4.1% | `core/src/slice/iter/macros.rs:<stacks_marf::cache::SharedLru<(stacks_common::types::chainstate::StacksBlockId,` |
| 39,976,960 | 2.9% | `core/src/intrinsics/mod.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 32,544,850 | 2.3% | `stacks-marf/src/node.rs:<stacks_marf::node::TriePtr>::from_slice_compressed` |
| 30,474,240 | 2.2% | `crates/core_arch/src/x86/sse2.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 22,077,440 | 1.6% | `core/src/num/uint_macros.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |

**lateprobe-4096-patchednodes64-warm**

| Ir (self) | share | function (file) |
|---|---|---|
| 88,101,555 | 8.4% | `./string/../sysdeps/x86_64/multiarch/memmove-vec-unaligned-erms.S:__memcpy_avx_unaligned_erms` |
| 76,824,175 | 7.3% | `stacks-marf/src/node.rs:<stacks_marf::node::TriePtr>::from_slice_compressed` |
| 69,263,360 | 6.6% | `sha2-0.10.9/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 47,573,520 | 4.5% | `stacks-marf/src/bits.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 39,976,960 | 3.8% | `core/src/intrinsics/mod.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 30,474,240 | 2.9% | `crates/core_arch/src/x86/sse2.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 25,647,215 | 2.5% | `core/src/num/uint_macros.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 22,077,440 | 2.1% | `core/src/num/uint_macros.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |

**lateprobe-4096-patchfree-warm**

| Ir (self) | share | function (file) |
|---|---|---|
| 82,057,310 | 23.3% | `stacks-marf/src/node.rs:<stacks_marf::node::TriePtr>::from_slice_compressed` |
| 25,089,180 | 7.1% | `./string/../sysdeps/x86_64/multiarch/memmove-vec-unaligned-erms.S:__memcpy_avx_unaligned_erms` |
| 16,910,000 | 4.8% | `sha2-0.10.9/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 11,172,330 | 3.2% | `core/src/num/uint_macros.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 10,588,125 | 3.0% | `core/src/result.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 10,167,250 | 2.9% | `stacks-marf/src/bits.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 9,760,000 | 2.8% | `core/src/intrinsics/mod.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 7,941,030 | 2.3% | `core/src/slice/mod.rs:<stacks_marf::node::TriePtr>::from_slice_compressed` |

**lateprobe-64-patchednodes1024-warm**

| Ir (self) | share | function (file) |
|---|---|---|
| 172,103,745 | 10.5% | `stacks-marf/src/node.rs:<stacks_marf::scratch::MarfReadState>::apply_patches_to_node::<stacks_marf::node::Trie` |
| 133,785,700 | 8.2% | `stacks-marf/src/node.rs:<stacks_marf::node::TriePtr>::from_slice_compressed` |
| 128,772,037 | 7.9% | `./string/../sysdeps/x86_64/multiarch/memmove-vec-unaligned-erms.S:__memcpy_avx_unaligned_erms` |
| 76,072,865 | 4.6% | `stacks-marf/src/bits.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 69,263,360 | 4.2% | `sha2-0.10.9/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 50,999,485 | 3.1% | `core/src/ptr/non_null.rs:<stacks_marf::scratch::MarfReadState>::apply_patches_to_node::<stacks_marf::node::Tri` |
| 40,482,555 | 2.5% | `core/src/num/uint_macros.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 39,976,960 | 2.4% | `core/src/intrinsics/mod.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |

**lateprobe-64-patchednodes64-warm**

| Ir (self) | share | function (file) |
|---|---|---|
| 89,032,635 | 8.5% | `./string/../sysdeps/x86_64/multiarch/memmove-vec-unaligned-erms.S:__memcpy_avx_unaligned_erms` |
| 76,824,175 | 7.3% | `stacks-marf/src/node.rs:<stacks_marf::node::TriePtr>::from_slice_compressed` |
| 69,263,360 | 6.6% | `sha2-0.10.9/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 47,573,520 | 4.5% | `stacks-marf/src/bits.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 39,976,960 | 3.8% | `core/src/intrinsics/mod.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 30,474,240 | 2.9% | `crates/core_arch/src/x86/sse2.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 25,647,215 | 2.5% | `core/src/num/uint_macros.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 22,077,440 | 2.1% | `core/src/num/uint_macros.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |

**lateprobe-64-patchfree-warm**

| Ir (self) | share | function (file) |
|---|---|---|
| 82,057,310 | 23.3% | `stacks-marf/src/node.rs:<stacks_marf::node::TriePtr>::from_slice_compressed` |
| 25,069,190 | 7.1% | `./string/../sysdeps/x86_64/multiarch/memmove-vec-unaligned-erms.S:__memcpy_avx_unaligned_erms` |
| 16,910,000 | 4.8% | `sha2-0.10.9/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 11,172,330 | 3.2% | `core/src/num/uint_macros.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 10,588,125 | 3.0% | `core/src/result.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 10,167,250 | 2.9% | `stacks-marf/src/bits.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 9,760,000 | 2.8% | `core/src/intrinsics/mod.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 7,941,030 | 2.3% | `core/src/slice/mod.rs:<stacks_marf::node::TriePtr>::from_slice_compressed` |

**reference-4096-patchednodes1024-warm**

| Ir (self) | share | function (file) |
|---|---|---|
| 644,493,600 | 26.6% | `core/src/slice/cmp.rs:<stacks_marf::cache::SharedLru<(stacks_common::types::chainstate::StacksBlockId, u64), (` |
| 438,143,435 | 18.1% | `core/src/slice/iter/macros.rs:<stacks_marf::cache::SharedLru<(stacks_common::types::chainstate::StacksBlockId,` |
| 403,243,445 | 16.6% | `./string/../sysdeps/x86_64/multiarch/memmove-vec-unaligned-erms.S:__memcpy_avx_unaligned_erms` |
| 143,362,780 | 5.9% | `core/src/ptr/non_null.rs:<stacks_marf::cache::SharedLru<(stacks_common::types::chainstate::StacksBlockId, u64)` |
| 69,263,360 | 2.9% | `sha2-0.10.9/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 39,976,960 | 1.6% | `core/src/intrinsics/mod.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 32,544,850 | 1.3% | `stacks-marf/src/node.rs:<stacks_marf::node::TriePtr>::from_slice_compressed` |
| 30,474,240 | 1.3% | `crates/core_arch/src/x86/sse2.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |

**reference-4096-patchednodes64-cold**

| Ir (self) | share | function (file) |
|---|---|---|
| 15,953,718 | 7.1% | `./string/../sysdeps/x86_64/multiarch/memmove-vec-unaligned-erms.S:__memcpy_avx_unaligned_erms` |
| 15,606,027 | 7.0% | `stacks-marf/src/node.rs:<stacks_marf::node::TriePtr>::from_slice_compressed` |
| 13,852,672 | 6.2% | `sha2-0.10.9/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 10,008,702 | 4.5% | `core/src/slice/cmp.rs:<stacks_marf::cache::SharedLru<(stacks_common::types::chainstate::StacksBlockId, u64), (` |
| 9,642,074 | 4.3% | `stacks-marf/src/bits.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 7,995,392 | 3.6% | `core/src/intrinsics/mod.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 6,973,036 | 3.1% | `core/src/slice/iter/macros.rs:<stacks_marf::cache::SharedLru<(stacks_common::types::chainstate::StacksBlockId,` |
| 6,094,848 | 2.7% | `crates/core_arch/src/x86/sse2.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |

**reference-4096-patchednodes64-warm**

| Ir (self) | share | function (file) |
|---|---|---|
| 78,489,195 | 7.1% | `./string/../sysdeps/x86_64/multiarch/memmove-vec-unaligned-erms.S:__memcpy_avx_unaligned_erms` |
| 76,824,175 | 7.0% | `stacks-marf/src/node.rs:<stacks_marf::node::TriePtr>::from_slice_compressed` |
| 69,263,360 | 6.3% | `sha2-0.10.9/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 50,239,530 | 4.5% | `core/src/slice/cmp.rs:<stacks_marf::cache::SharedLru<(stacks_common::types::chainstate::StacksBlockId, u64), (` |
| 47,573,520 | 4.3% | `stacks-marf/src/bits.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 39,976,960 | 3.6% | `core/src/intrinsics/mod.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 34,999,350 | 3.2% | `core/src/slice/iter/macros.rs:<stacks_marf::cache::SharedLru<(stacks_common::types::chainstate::StacksBlockId,` |
| 30,474,240 | 2.8% | `crates/core_arch/src/x86/sse2.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |

**reference-4096-patchfree-warm**

| Ir (self) | share | function (file) |
|---|---|---|
| 82,057,310 | 23.2% | `stacks-marf/src/node.rs:<stacks_marf::node::TriePtr>::from_slice_compressed` |
| 25,089,180 | 7.1% | `./string/../sysdeps/x86_64/multiarch/memmove-vec-unaligned-erms.S:__memcpy_avx_unaligned_erms` |
| 16,910,000 | 4.8% | `sha2-0.10.9/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 11,172,330 | 3.2% | `core/src/num/uint_macros.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 10,588,125 | 3.0% | `core/src/result.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 10,167,250 | 2.9% | `stacks-marf/src/bits.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 9,760,000 | 2.8% | `core/src/intrinsics/mod.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 7,941,030 | 2.2% | `core/src/slice/mod.rs:<stacks_marf::node::TriePtr>::from_slice_compressed` |

**reference-64-patchednodes1024-cold**

| Ir (self) | share | function (file) |
|---|---|---|
| 34,472,399 | 10.2% | `stacks-marf/src/node.rs:<stacks_marf::scratch::MarfReadState>::apply_patches_to_node::<stacks_marf::node::Trie` |
| 26,800,159 | 7.9% | `stacks-marf/src/node.rs:<stacks_marf::node::TriePtr>::from_slice_compressed` |
| 24,075,145 | 7.1% | `./string/../sysdeps/x86_64/multiarch/memmove-vec-unaligned-erms.S:__memcpy_avx_unaligned_erms` |
| 15,233,124 | 4.5% | `stacks-marf/src/bits.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 13,852,672 | 4.1% | `sha2-0.10.9/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 11,263,419 | 3.3% | `core/src/slice/cmp.rs:<stacks_marf::cache::SharedLru<(stacks_common::types::chainstate::StacksBlockId, u64), (` |
| 10,214,653 | 3.0% | `core/src/ptr/non_null.rs:<stacks_marf::scratch::MarfReadState>::apply_patches_to_node::<stacks_marf::node::Tri` |
| 8,106,940 | 2.4% | `core/src/num/uint_macros.rs:stacks_marf::bits::ptrs_from_parts_into` |

**reference-64-patchednodes1024-warm**

| Ir (self) | share | function (file) |
|---|---|---|
| 172,103,745 | 10.2% | `stacks-marf/src/node.rs:<stacks_marf::scratch::MarfReadState>::apply_patches_to_node::<stacks_marf::node::Trie` |
| 133,785,700 | 7.9% | `stacks-marf/src/node.rs:<stacks_marf::node::TriePtr>::from_slice_compressed` |
| 120,397,901 | 7.1% | `./string/../sysdeps/x86_64/multiarch/memmove-vec-unaligned-erms.S:__memcpy_avx_unaligned_erms` |
| 76,072,865 | 4.5% | `stacks-marf/src/bits.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 69,263,360 | 4.1% | `sha2-0.10.9/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 56,545,110 | 3.4% | `core/src/slice/cmp.rs:<stacks_marf::cache::SharedLru<(stacks_common::types::chainstate::StacksBlockId, u64), (` |
| 50,999,485 | 3.0% | `core/src/ptr/non_null.rs:<stacks_marf::scratch::MarfReadState>::apply_patches_to_node::<stacks_marf::node::Tri` |
| 40,482,555 | 2.4% | `core/src/num/uint_macros.rs:stacks_marf::bits::ptrs_from_parts_into` |

**reference-64-patchednodes64-cold**

| Ir (self) | share | function (file) |
|---|---|---|
| 15,780,880 | 7.1% | `./string/../sysdeps/x86_64/multiarch/memmove-vec-unaligned-erms.S:__memcpy_avx_unaligned_erms` |
| 15,606,027 | 7.0% | `stacks-marf/src/node.rs:<stacks_marf::node::TriePtr>::from_slice_compressed` |
| 13,852,672 | 6.2% | `sha2-0.10.9/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 10,008,702 | 4.5% | `core/src/slice/cmp.rs:<stacks_marf::cache::SharedLru<(stacks_common::types::chainstate::StacksBlockId, u64), (` |
| 9,642,074 | 4.3% | `stacks-marf/src/bits.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 7,995,392 | 3.6% | `core/src/intrinsics/mod.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 6,973,036 | 3.1% | `core/src/slice/iter/macros.rs:<stacks_marf::cache::SharedLru<(stacks_common::types::chainstate::StacksBlockId,` |
| 6,094,848 | 2.7% | `crates/core_arch/src/x86/sse2.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |

**reference-64-patchednodes64-warm**

| Ir (self) | share | function (file) |
|---|---|---|
| 78,489,215 | 7.1% | `./string/../sysdeps/x86_64/multiarch/memmove-vec-unaligned-erms.S:__memcpy_avx_unaligned_erms` |
| 76,824,175 | 7.0% | `stacks-marf/src/node.rs:<stacks_marf::node::TriePtr>::from_slice_compressed` |
| 69,263,360 | 6.3% | `sha2-0.10.9/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 50,239,530 | 4.5% | `core/src/slice/cmp.rs:<stacks_marf::cache::SharedLru<(stacks_common::types::chainstate::StacksBlockId, u64), (` |
| 47,573,520 | 4.3% | `stacks-marf/src/bits.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 39,976,960 | 3.6% | `core/src/intrinsics/mod.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 34,999,350 | 3.2% | `core/src/slice/iter/macros.rs:<stacks_marf::cache::SharedLru<(stacks_common::types::chainstate::StacksBlockId,` |
| 30,474,240 | 2.8% | `crates/core_arch/src/x86/sse2.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |

**reference-64-patchfree-warm**

| Ir (self) | share | function (file) |
|---|---|---|
| 82,057,310 | 23.2% | `stacks-marf/src/node.rs:<stacks_marf::node::TriePtr>::from_slice_compressed` |
| 25,089,180 | 7.1% | `./string/../sysdeps/x86_64/multiarch/memmove-vec-unaligned-erms.S:__memcpy_avx_unaligned_erms` |
| 16,910,000 | 4.8% | `sha2-0.10.9/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 11,172,330 | 3.2% | `core/src/num/uint_macros.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 10,588,125 | 3.0% | `core/src/result.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 10,167,250 | 2.9% | `stacks-marf/src/bits.rs:stacks_marf::bits::ptrs_from_parts_into` |
| 9,760,000 | 2.8% | `core/src/intrinsics/mod.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 7,941,030 | 2.2% | `core/src/slice/mod.rs:<stacks_marf::node::TriePtr>::from_slice_compressed` |

**rewrite-4096-patchednodes1024-cold**

| Ir (self) | share | function (file) |
|---|---|---|
| 13,852,672 | 15.5% | `sha2-0.10.8/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 7,995,392 | 8.9% | `core/src/intrinsics/mod.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 7,179,534 | 8.0% | `stacks-marf-next/src/codec.rs:<stacks_marf_next::codec::BranchView>::skip_scan` |
| 6,094,848 | 6.8% | `crates/core_arch/src/x86/sse2.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 4,415,488 | 4.9% | `core/src/num/uint_macros.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 4,088,656 | 4.6% | `core/src/hash/sip.rs:<std::hash::random::DefaultHasher as core::hash::Hasher>::write` |
| 3,178,496 | 3.5% | `core/src/cmp.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 1,885,295 | 2.1% | `stacks-marf-next/src/storage.rs:stacks_marf_next::storage::resolve_in` |

**rewrite-4096-patchednodes1024-warm**

| Ir (self) | share | function (file) |
|---|---|---|
| 69,263,360 | 17.0% | `sha2-0.10.8/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 39,976,960 | 9.8% | `core/src/intrinsics/mod.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 35,897,670 | 8.8% | `stacks-marf-next/src/codec.rs:<stacks_marf_next::codec::BranchView>::skip_scan` |
| 30,474,240 | 7.5% | `crates/core_arch/src/x86/sse2.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 22,077,440 | 5.4% | `core/src/num/uint_macros.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 19,023,625 | 4.7% | `core/src/hash/sip.rs:<std::hash::random::DefaultHasher as core::hash::Hasher>::write` |
| 15,892,480 | 3.9% | `core/src/cmp.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 8,569,140 | 2.1% | `stacks-marf-next/src/storage.rs:stacks_marf_next::storage::resolve_in` |

**rewrite-4096-patchednodes64-cold**

| Ir (self) | share | function (file) |
|---|---|---|
| 16,821,296 | 17.4% | `stacks-marf-next/src/codec.rs:<stacks_marf_next::codec::BranchView>::skip_scan` |
| 13,852,672 | 14.3% | `sha2-0.10.8/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 7,995,392 | 8.3% | `core/src/intrinsics/mod.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 6,094,848 | 6.3% | `crates/core_arch/src/x86/sse2.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 4,415,488 | 4.6% | `core/src/num/uint_macros.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 3,658,567 | 3.8% | `core/src/hash/sip.rs:<std::hash::random::DefaultHasher as core::hash::Hasher>::write` |
| 3,178,496 | 3.3% | `core/src/cmp.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 1,886,306 | 1.9% | `core/src/slice/index.rs:<stacks_marf_next::codec::BranchView>::skip_scan` |

**rewrite-4096-patchednodes64-warm**

| Ir (self) | share | function (file) |
|---|---|---|
| 84,106,480 | 17.7% | `stacks-marf-next/src/codec.rs:<stacks_marf_next::codec::BranchView>::skip_scan` |
| 69,263,360 | 14.6% | `sha2-0.10.8/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 39,976,960 | 8.4% | `core/src/intrinsics/mod.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 30,474,240 | 6.4% | `crates/core_arch/src/x86/sse2.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 22,077,440 | 4.7% | `core/src/num/uint_macros.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 18,192,470 | 3.8% | `core/src/hash/sip.rs:<std::hash::random::DefaultHasher as core::hash::Hasher>::write` |
| 15,892,480 | 3.3% | `core/src/cmp.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 9,431,530 | 2.0% | `core/src/slice/index.rs:<stacks_marf_next::codec::BranchView>::skip_scan` |

**rewrite-4096-patchfree-warm**

| Ir (self) | share | function (file) |
|---|---|---|
| 49,544,075 | 32.7% | `stacks-marf-next/src/codec.rs:<stacks_marf_next::codec::BranchView>::skip_scan` |
| 16,910,000 | 11.2% | `sha2-0.10.8/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 10,628,355 | 7.0% | `core/src/slice/index.rs:<stacks_marf_next::codec::BranchView>::skip_scan` |
| 9,760,000 | 6.5% | `core/src/intrinsics/mod.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 7,440,000 | 4.9% | `crates/core_arch/src/x86/sse2.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 5,390,000 | 3.6% | `core/src/num/uint_macros.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 3,880,000 | 2.6% | `core/src/cmp.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 2,718,720 | 1.8% | `core/src/cmp.rs:<stacks_marf_next::codec::BranchView>::skip_scan` |

**rewrite-64-patchednodes1024-warm**

| Ir (self) | share | function (file) |
|---|---|---|
| 158,975,940 | 13.3% | `stacks-marf-next/src/codec.rs:<stacks_marf_next::codec::BranchView>::fill_with_end` |
| 77,992,758 | 6.5% | `stacks-marf-next/src/node.rs:stacks_marf_next::codec::apply_patch` |
| 75,776,214 | 6.3% | `stacks-marf-next/src/codec.rs:<stacks_marf_next::codec::BranchView>::fill_with_end::{closure#0}` |
| 69,263,360 | 5.8% | `sha2-0.10.8/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 46,696,644 | 3.9% | `stacks-marf-next/src/codec.rs:stacks_marf_next::codec::apply_patch` |
| 45,836,250 | 3.8% | `core/src/num/uint_macros.rs:<stacks_marf_next::codec::BranchView>::fill_with_end` |
| 41,457,030 | 3.5% | `core/src/intrinsics/mod.rs:<stacks_marf_next::codec::BranchView>::fill_with_end` |
| 39,976,960 | 3.4% | `core/src/intrinsics/mod.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |

**rewrite-64-patchednodes64-cold**

| Ir (self) | share | function (file) |
|---|---|---|
| 16,821,296 | 16.9% | `stacks-marf-next/src/codec.rs:<stacks_marf_next::codec::BranchView>::skip_scan` |
| 13,852,672 | 13.9% | `sha2-0.10.8/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 7,995,392 | 8.0% | `core/src/intrinsics/mod.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 6,094,848 | 6.1% | `crates/core_arch/src/x86/sse2.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 4,415,488 | 4.4% | `core/src/num/uint_macros.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 3,679,578 | 3.7% | `core/src/hash/sip.rs:<std::hash::random::DefaultHasher as core::hash::Hasher>::write` |
| 3,178,496 | 3.2% | `core/src/cmp.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 1,886,306 | 1.9% | `core/src/slice/index.rs:<stacks_marf_next::codec::BranchView>::skip_scan` |

**rewrite-64-patchednodes64-warm**

| Ir (self) | share | function (file) |
|---|---|---|
| 84,106,480 | 17.2% | `stacks-marf-next/src/codec.rs:<stacks_marf_next::codec::BranchView>::skip_scan` |
| 69,263,360 | 14.1% | `sha2-0.10.8/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 39,976,960 | 8.2% | `core/src/intrinsics/mod.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 30,474,240 | 6.2% | `crates/core_arch/src/x86/sse2.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 22,077,440 | 4.5% | `core/src/num/uint_macros.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 18,301,900 | 3.7% | `core/src/hash/sip.rs:<std::hash::random::DefaultHasher as core::hash::Hasher>::write` |
| 15,892,480 | 3.2% | `core/src/cmp.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 9,431,530 | 1.9% | `core/src/slice/index.rs:<stacks_marf_next::codec::BranchView>::skip_scan` |

**rewrite-64-patchfree-warm**

| Ir (self) | share | function (file) |
|---|---|---|
| 49,544,075 | 32.7% | `stacks-marf-next/src/codec.rs:<stacks_marf_next::codec::BranchView>::skip_scan` |
| 16,910,000 | 11.2% | `sha2-0.10.8/src/sha512/x86.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 10,628,355 | 7.0% | `core/src/slice/index.rs:<stacks_marf_next::codec::BranchView>::skip_scan` |
| 9,760,000 | 6.4% | `core/src/intrinsics/mod.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 7,440,000 | 4.9% | `crates/core_arch/src/x86/sse2.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 5,390,000 | 3.6% | `core/src/num/uint_macros.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 3,880,000 | 2.6% | `core/src/cmp.rs:sha2::sha512::x86::sha512_compress_x86_64_avx2` |
| 2,718,720 | 1.8% | `core/src/cmp.rs:<stacks_marf_next::codec::BranchView>::skip_scan` |

