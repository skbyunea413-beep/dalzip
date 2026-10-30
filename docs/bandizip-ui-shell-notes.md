# Bandizip 7.47 — UI · 탐색기 연결 분석 메모 (2026-10-31)

정적 분석(REA/Ghidra, dumpbin, 리소스) + 레지스트리 조회(컨테이너 밖) + 화면 캡처(PrintWindow, 시작 화면과
아카이브 창; 반디집 화면이라 저장소에는 넣지 않음).
새로 압축 대화상자는 `Bandizip.exe a <폴더>`가 "잘못된 실행 파라미터"라 띄우지 못함 — 레이아웃은 lang/resource.h에서 복원.

아카이브 창(캡처): 파란 툴바 + 흰 선 아이콘(열기▾ 풀기▾ 새로 압축 파일 추가 파일 삭제 테스트 스캔 칼럼 설정 코드페이지),
왼쪽 트리(아카이브 → 폴더), 오른쪽 목록, 상태 표시줄 "파일: N, 폴더: M, 압축 파일 크기: X (솔리드, 85.4%)".
루트 폴더가 하나뿐이면 그 안으로 들어가서 보여 줌(enterFolderWhenRootFolderIsOnlyOne).

## 탐색기 연결
- 클래식 메뉴: `bdzshl.x64.dll`, ATL COM `CAABdzCtx` {5B69A6B4-393B-459C-8EBB-214237A9E7AC}
  - 인터페이스(RTTI): IShellExtInit, IContextMenu/2/3(오너드로 아이콘), IPersistFile+IDropTarget(DropHandler)
  - 등록: `HKCR\{*,Directory,Directory\Background}\shellex\ContextMenuHandlers\AABdzCtx` ("AA"=정렬상 앞쪽),
    `HKCR\{Directory,Drive}\shellex\DragDropHandlers\AABdzCtx`(오른쪽 드래그 메뉴), InprocServer32 Apartment
  - 확장자: `.zip` 기본값 → `Bandizip.zip`, 이전 값은 `Bandizip_bak`에 백업. ProgID에 DefaultIcon(icons\default\zip.ico),
    shell\open\command `"Bandizip.exe" "%1"`, shellex\DropHandler = 같은 CLSID(아카이브 위에 끌어놓으면 추가)
- Win11 새 메뉴: `data\bzshell.x64.msix` = sparse 패키지(`uap10:AllowExternalContent`, Bandisoft 서명)
  - `windows.fileExplorerContextMenus`: `*`·`Directory` → CLSID 0001DEAD-…0001, `Directory\Background` → …0002
  - `com:SurrogateServer`로 `bzshell.x64.dll`(IExplorerCommand) 로드. 설치 확인: Get-AppxPackage BandizipShellext2 1.0.0.3
- 셸 DLL → 본체 명령 프로토콜(문자열):
  - 풀기 `bx -target:auto|name|dlg -o:<dir> <archive>`, 압축 `bc [-fmt:7z|gz|xz] -o:<archive> …`, 테스트 `bt`, 열기 `/open`
  - 옵션 `-delsrc`, `-email`, `-storeroot:yes`
  - 파일이 많으면 `/usemmf <name>`: 페이지파일 기반 이름 있는 공유메모리 `bzfmmf%u%u` (PAGE_READWRITE, FUN_180012380).
    공유메모리 내부 레이아웃은 확인 안 함(추정: 경로 목록)
- 메뉴 항목·기본값: HKCU\Software\Bandizip `shell_*` (ExtractHere/Auto/Sub/Dlg, OpenArchive, ArchivingAsFileName,
  ArchivingEach, ArchivingPopup, Archiving7z, UseDropHandler, ShowSubmenu=0 → 기본은 평면 메뉴)
- 메뉴 문구(langs\Korean.lang [SHELL]): 여기에 풀기 / 알아서 풀기 / ${name}\ 에 풀기 / 반디집으로 압축 풀기... /
  ${name}으로 압축하기 / 각각 파일명/폴더명으로 압축하기 / 반디집으로 압축하기... / 압축 파일 테스트 / 반디집으로 열기

## UI
- 순수 Win32 + GDI + comctl32(ListView, ImageList) + UxTheme. D2D·WebView 없음. 커스텀 그리기.
- `data\skin.data` = 무압축 ZIP, PNG 158개(tb_btn_open/create/add/extract/delete/test/view/codepage/virus, 큰 타일 버튼 등)
- `data\resource.data` = 개발용 resource.h 그대로 → 대화상자 ID 목록(IDD_ARCHIVE, IDD_EXTRACT, IDD_PASSWORD_INPUT,
  IDD_ARCHIVE_ADVANCED_7Z/ZIP/TAR, IDD_SETTING_SHELL, IDD_PASSMAN 등)
- 시작 화면: 파란 그라데이션, 큰 흰 타일 2개(압축 파일 열기 / 새로 압축하기), 아래 띠 배너
- 아카이브 창(설정 키): 왼쪽 트리 패널(304px@96dpi), 열 = 이름·압축 크기·원래 크기·종류·수정 날짜(+방식·암호·CRC…),
  미리보기·주석 패널, 상태 표시줄, 툴바
- 새로 압축(IDD_ARCHIVE): 압축할 파일 목록(추가/제거), 파일 이름, 저장 폴더, 압축 형식, 분할 압축, 압축 방법, 암호 설정...,
  압축 후 테스트, 압축 후 원본 삭제, 각각 파일명/폴더명으로 압축, 상세 설정 보기, 압축 시작
- 풀기 진행(IDD_EXTRACT): 중지 / 일시 정지 / 닫기
