# Windows 터미널 수동 검사

실제 IME 조합과 화면 표시는 자동 ConPTY 테스트만으로 확인할 수 없습니다. 아래 **6개 조합**을 하나씩 검사합니다. 아직 최종 통과한 조합은 없습니다. WT의 세 셸에서 detach/quit 후 입력 모드 `487→480` 문제가 확인됐습니다. 수정본은 kernel 회귀 검사와 Windows 전체 workspace 검사 두 변형을 통과했으며, 아래 경로로 실제 UI를 재검사합니다. 최초 실행의 pwsh 경로 중복 오류도 `$PSHOME/pwsh.exe` 단일 경로로 수정했습니다.

| 셸 (`-Shell`) | Windows Terminal (`wt`) | native console (`native`) |
| --- | --- | --- |
| `pwsh` | 화면 복원 관찰 / 입력 mode 불일치, 재검사 필요 | 미검증 |
| `cmd` | 입력 mode 불일치 / 구 paste 절차 오류, 재검사 필요 | 미검증 |
| `winps-cooked` | 화면 복원 관찰 / 입력 mode 불일치, 재검사 필요 | 미검증 |

`winps-cooked`는 해당 자식 프로세스에서만 PSReadLine을 제거합니다. PowerShell 5.1 + PSReadLine 2.4.5의 이모지 입력 실패는 별도의 알려진 제약이며, 이 검사가 그 문제를 해결했다고 의미하지 않습니다.

## 실행

기존 창/프로필을 수정하지 않고 새 창에서 실행합니다. **dot-source하지 마세요.** 관리자 권한은 필요 없습니다. `-Terminal`은 실제 호스트를 생성하지 않으므로 아래 명령으로 호스트를 선택해야 합니다.

**현재 Cargo 설치 파일은 사용 중이라 교체하지 못했습니다.** 아래 명령의 `-Binary`는 검증된 새 release를 직접 지정합니다. SHA256: `9F55CA9198D6A15365D054B201EC2606016814F92A4BA510D8945B7FF4C2ADD8`.

```powershell
# Windows Terminal / PowerShell 7
wt.exe -w new new-tab pwsh.exe -NoLogo -NoProfile -File D:/Git/zw/zellij-integration-tests/manual/windows-ui.ps1 -Terminal wt -Binary D:/Git/zw/target/release/zellij.exe -Shell pwsh

# native console / PowerShell 7
conhost.exe pwsh.exe -NoLogo -NoProfile -File D:/Git/zw/zellij-integration-tests/manual/windows-ui.ps1 -Terminal native -Binary D:/Git/zw/target/release/zellij.exe -Shell pwsh
```

각 명령의 마지막 `pwsh`만 `cmd`, `winps-cooked`로 바꾸어 나머지 조합을 검사합니다. `conhost.exe` 창이 실제 고전 콘솔 창인지도 확인합니다. 호스트에 의해 다른 터미널로 전환되었다면 native 검증으로 기록하지 마세요. 도우미는 `-Terminal native`인 자식 프로세스에서만 `TERM`/`WT_SESSION`을 제거하여 실제 native INPUT_RECORD 입력 경로를 선택합니다. 부모 셸 환경은 변경하지 않습니다.

도우미는 고유한 `zw-ui-*` 세션과 임시 설정/레이아웃을 사용합니다. 세션 이름 인덱스와의 일관성을 위해 IPC와 cache는 일반 Zellij 경로를 유지합니다. 명령 자동 실행/PowerShell 프로필을 비활성화하고, pwsh의 이 검사 프로세스에서만 기록 저장을 끕니다. 다른 세션을 종료하지 않으며, 정상 종료 시 자기 테스트 세션만 삭제합니다. 결과 디렉터리는 `%TEMP%/zw-ui-*`이며 삭제하지 않고 보존합니다. 일반 Zellij plugin cache는 공유될 수 있습니다.

## 각 창에서 확인

1. 시작 안내에서 Enter를 누릅니다. **직접 한글 IME를 사용해** `echo 한글끝`을 입력합니다. 조합 중 표시·후보 창 위치·확정 후 문자열을 관찰합니다.
2. `echo 한글😀끝`을 입력/붙여넣어 봅니다. 문자 누락·`�`·겹침·커서 위치 오류가 없는지 확인합니다. emoji가 사각형이면 폰트와 호스트 이름을 기록하고 통과로 판단하지 않습니다.
3. Ctrl+g를 두 번, Alt+l(소문자 L)을 두 번 누릅니다. 하단 상태 표시가 각각 Locked/Normal로 전환되는지 확인합니다. 호스트 단축키에 가로채이면 해당 조합을 기록합니다.
4. 아래 대량 붙여넣기를 실행합니다.
5. 창을 좁혔다 넓혀 한글/emoji 줄과 프롬프트가 정상 갱신되는지, 키 입력에 계속 응답하는지 확인합니다.
6. Normal 모드에서 **Ctrl+d**로 detach합니다. 도우미의 `console modes/code pages restored=True`를 확인합니다. 이전 화면으로 돌아왔는지, 커서·입력 echo·Enter가 정상인지 직접 확인합니다.
7. Enter로 재접속합니다. 이전 셸/화면이 유지되는지 확인한 뒤 Normal 모드에서 **Ctrl+q**로 종료합니다. 두 번째 복원 결과와 커서·echo·화면 복원을 확인합니다.

### 대량 붙여넣기

검사 셸의 현재 디렉터리에 `paste.txt`가 있습니다. 다음 명령은 **클립보드를 검사 문자열로 바꾸므로**, 필요한 기존 클립보드 내용은 먼저 보관하세요.

```powershell
pwsh.exe -NoProfile -File copy-paste.ps1
```

PowerShell 셸에서는 아래 명령을 실행한 뒤 입력 대기 중 붙여넣고 Enter를 누릅니다. 결과가 `True`인지 확인합니다.

```powershell
$p = Read-Host 'PASTE'; $p -ceq [IO.File]::ReadAllText('paste.txt')
```

cmd의 `set /p`는 이번 환경에서 1023자만 읽었습니다. 이전 안내의 이 명령으로는 긴 문자열의 나머지가 명령으로 실행되어 `not recognized` 및 비교 실패가 날 수 있으므로 **사용하지 않습니다**.

cmd에서는 아래 명령으로 UTF-8을 설정하고, 테스트 문자열을 `pasted.txt`에 쓰는 안전한 `echo` 명령 전체를 클립보드에 넣습니다. **기본 cmd 프롬프트에서 붙여넣고 Enter를 누른 뒤** 비교 명령을 실행합니다.

```bat
chcp 65001
pwsh.exe -NoProfile -File copy-paste.ps1 -ForCmd
```

붙여넣기와 Enter 후:

```bat
pwsh.exe -NoProfile -File check-paste.ps1
```

결과가 `True`여야 합니다. ASCII+한글+emoji와 echo의 CRLF까지 비교합니다. 이 수정된 명령/파일 비교는 별도로 자동 확인했지만, 실제 터미널 붙여넣기 동작의 통과를 대신하지 않습니다.

문자열은 ASCII 3840자 + `한글😀끝`이며 UTF-8로 3853바이트입니다. 실제 붙여넣기 중 UI가 멈추거나 내용이 잘리면 실패입니다. 4MiB 정체/취소 처리는 별도 자동 테스트로 검증합니다.

## 결과 전달

각 조합에 대해 `IME / emoji 표시 / 붙여넣기 / Ctrl·Alt / resize / detach 복원 / quit 복원`의 통과 또는 실패를 알려주세요. `case.json`은 실행 파일 경로·SHA256·호스트 레이블을, `detach.json`과 `quit.json`은 콘솔 mode/code page와 종료 코드를 기록합니다. JSON만으로 시각 검사를 통과 처리하지 않습니다.

실패하면 창/셸, 재현 단계, 보이는 증상, 결과 디렉터리를 함께 알려주세요. 도우미 실행 전 오류 또는 중간 창 강제 종료도 미검증으로 기록합니다.
