# Bandizip 7.47 (build 71912) — Ark 엔진 분석 메모

대상: `ark.x64.dll` 7.47.0.1 (sha256 868a4b87…bb135), `ark.x64.lgpl.dll`, `bz.exe`
도구: REA 6.4.0 + Ghidra 12.1.4 (정적 분석만, 실행 관찰 없음)

## 구조 (관찰)
- export 는 `CreateArk`, `CreateArkCompressor` 두 개뿐. COM 비슷한 인터페이스 팩토리
  (`IArkCompressor`, `IArkCompressorEx` RTTI). `CreateArkCompressor(0x20250923)` — 인자는
  API 버전 매직(날짜형), 틀리면 NULL. 0x5b0 바이트 객체 할당 후 생성자 호출.
- 7-Zip 계열(7z/xz/wim 핸들러, CoderMixer2 MT, LZMA/LZMA2/BCJ/BCJ2/Delta/ARM64/PPMd)은
  LGPL 분리 DLL(`ark.x64.lgpl.dll`)에 있음.
- 번들 라이브러리(ArkLicense.txt): zlib 1.3, bzip2, LZMA SDK 23.01, zstd 1.4.3, lz4/lz5/lizard,
  zopfli, ZPAQ 7.15(libzpaq + jidac), PPMd, packMP3, WavPack, AES(Gladman), LEA(KISA), unrar 등.

## 압축 쪽 특징 (관찰)
- ZPAQ 생성: `CJidacWriter`, `CompressJob`, `WriterPair`, `CArkParaZpaqMan` — zpaq 7.15 의
  JIDAC 저널 포맷(중복 제거 + 블록 병렬 압축)을 그대로 씀. 반디집에서 dedup 은 ZPAQ 에만 있음.
- ZIP: Deflate/Deflate64/zopfli(`-zopfli` 옵션), 암호 ZipCrypto·AES128/192/256·LEA128/256,
  `CArkZipRepair`(ZIP 복구).
- LZMA2 설정 함수 `FUN_180066450` (로그 문자열 "@@rLZMA2 out opt …" 근처):
  - level 미지정 → 5
  - thread 미지정 → CPU 개수, 1..64 로 clamp
  - 사전 크기 `FUN_180066300(level)`:
    상한 L0–2 64KB, L3–4 1MB, L5–6 16MB, L7–8 32MB, L9 64MB.
    2^n, 3·2^(n-1) 후보(1MB~1.5GB) 중 상한 이하이고 예상 메모리 ≤ (물리RAM − 16MB) 인 최대값.
  - 메모리 예측 `FUN_180066180`: 7-Zip LZMA2-MT 추정식과 같은 꼴(해시 테이블 크기 + BT4 son
    배열(level>4) + 블록 버퍼 × 스레드).
  - 옵션 플래그(+0x80)가 켜져 있으면 예상 메모리가 가용 물리 메모리의 80% 이하가 될 때까지
    스레드를 1씩 줄임.
- 출력 포맷(bz.exe): zip, zipx, exe(SFX), tar, tgz, lzh, iso, 7z, gz, xz. 레벨 0..9 (기본 5).

## 재구현(dalzip)에 가져간 아이디어 / 넘어선 지점
- 가져감: 레벨→사전/블록 상한 표, "메모리 예측으로 스레드 수 줄이기" 정책.
- 넘어섬: dedup 을 느린 ZPAQ 전용이 아니라 모든 레벨에 (FastCDC + BLAKE3),
  블록별 코덱 자동 선택(zstd / LZMA2 / PPMd 인코더 / store), 블록 종류별 LZMA2 파라미터,
  사전 상한 256MB, BCJ식 E8/E9 필터, Argon2id + XChaCha20-Poly1305 인증 암호(파일명 포함).
- 디컴파일 코드는 옮기지 않음. 동작·정책만 참고.
