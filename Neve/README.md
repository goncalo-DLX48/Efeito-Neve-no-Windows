# SnowHush

Neve interativa sobre o ambiente de trabalho do Windows 10/11: um overlay
transparente, *click-through*, com partículas simuladas, acumulação estilizada
sobre o topo das janelas e da barra de tarefas, e um menu na bandeja do sistema.

Escrito em **Rust** com Win32 + Direct3D 11 + Direct2D + DirectComposition.
Não usa frameworks de UI, não tem janela principal e não bloqueia cliques.

---

## 1. Requisitos

| | |
|---|---|
| Sistema | Windows 10 1809+ ou Windows 11 (x64) |
| GPU | Direct3D 11 (se não houver GPU, há fallback automático para WARP/software) |
| Compilador | Rust 1.75+ (testado com 1.99.1) |
| Linker | MSVC (recomendado) **ou** mingw-w64 (toolchain GNU) |

## 2. Compilação

```powershell
# na raiz do projeto
cargo build --release
```

ou, com o script auxiliar (também corre os testes e permite lançar a app):

```powershell
.\build.ps1 -Test -Run
```

O executável fica em `target\release\snowhush.exe` (~380 KB).

### Notas sobre a toolchain

* **MSVC (recomendado)** — instale *Visual Studio Build Tools* com a carga
  “Desktop development with C++” e use `rustup default stable-msvc`.
* **GNU** — o `rustup` já traz `dlltool`/`gcc`/`ld` em
  `lib\rustlib\x86_64-pc-windows-gnu\bin\self-contained`, mas o `dlltool` precisa
  do assembler `as.exe` do **mingw-w64** (por exemplo, uma distribuição portátil
  [WinLibs](https://winlibs.com)) no `PATH`. O `build.ps1` acrescenta a pasta do
  rustup e avisa se faltar o `as.exe`.

## 3. Execução

```powershell
.\target\release\snowhush.exe
```

Opções de linha de comandos (úteis para atalhos ou para o arranque automático):

```
--intensity <off|light|steady|heavy|variable>   intensidade inicial (omissão: steady)
--no-accumulate                                 começa sem acumulação
--no-windows                                    a neve não pousa sobre janelas
--behind                                        começa atrás das janelas
--help, -h                                      mostra esta ajuda
```

A aplicação é de instância única: se já estiver a correr, um segundo processo
termina imediatamente.

## 4. Menu da bandeja

Clique direito no ícone de floco de neve (clique duplo liga/desliga a neve):

```
Snowfall ▸   Off / Light / Steady / Heavy / Variable
────────────────────────────────
✓ Accumulate        acumula neve sobre superfícies
  Melt Snow         derrete toda a neve acumulada
✓ Snow on Windows   a neve pode pousar sobre janelas
  Behind Windows    desenha a neve atrás das janelas (papel de parede animado)
✓ Open at Login     HKCU\...\CurrentVersion\Run
────────────────────────────────
  Quit
```

`Open at Login` cria/remove o valor `SnowHush` em
`HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run` com o caminho
entre aspas do executável atual.

## 5. Arquitetura

```
src/
  main.rs        entrada; --help; log de pânicos; sem consola (subsistema windows)
  app.rs         estado, loop de mensagens/frames, comandos do menu, arranque
  overlay.rs     HWND transparente que cobre o ecrã virtual + WndProc
  renderer.rs    D3D11 → Direct2D → DirectComposition (desenho e apresentação)
  sim.rs         física das partículas, montes de neve, derretimento/deslize
  surfaces.rs    deteção de janelas e da barra de tarefas (Win32/DWM)
  tray.rs        Shell_NotifyIcon, ícone gerado em código, menu nativo
  autostart.rs   chave de registo “Open at Login”
  settings.rs    intensidades e densidades
  geometry.rs    retângulo 2D em píxeis físicos
  winutil.rs     utilitários Win32, logging opcional
```

Fluxo por frame (≈16,7 ms):

```
mensagens Win32 → tracker (7×/s) → simulação → Direct2D (GPU) → Present(1)
```

### 5.1 Overlay transparente e click-through

Uma única janela `WS_POPUP` cobre o **ecrã virtual inteiro** (todos os monitores,
incluindo coordenadas negativas) e usa:

| Estilo | Efeito |
|---|---|
| `WS_EX_NOREDIRECTIONBITMAP` | sem superfície GDI; o conteúdo vem só da composição |
| `WS_EX_LAYERED` + `WS_EX_TRANSPARENT` | todos os cliques atravessam a janela |
| `WS_EX_NOACTIVATE` | nunca rouba o foco |
| `WS_EX_TOOLWINDOW` | não aparece no Alt+Tab |
| `WS_EX_TOPMOST` | por cima das janelas (desligado no modo *Behind*) |

O processo é **Per-Monitor-V2**: `SetProcessDpiAwarenessContext` **e**
`SetThreadDpiAwarenessContext` (o contexto de DPI é fixado quando a thread é
criada, por isso a segunda chamada é necessária). Todo o desenho é feito em
píxeis físicos com `D2D1_UNIT_MODE_PIXELS`, pelo que a neve tem o mesmo tamanho
em monitores com escalas diferentes.

### 5.2 Deteção de janelas (`surfaces.rs`)

* `EnumWindows` devolve as janelas de topo **por ordem Z**.
* Filtros: `IsWindowVisible`, `IsIconic`,
  `DwmGetWindowAttribute(DWMWA_CLOAKED)` (desktops virtuais/UWP suspensas),
  `WS_CHILD`, `WS_EX_TOOLWINDOW`, `WS_EX_NOACTIVATE`, `WS_EX_TRANSPARENT`,
  classes internas do shell (`Progman`, `WorkerW`, `Shell_TrayWnd`, …) e a
  própria janela.
* Limites visíveis via `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)`, com
  `GetWindowRect` como *fallback* — evita que a neve flutue sobre a sombra DWM.
* Barra de tarefas: `FindWindowW("Shell_TrayWnd")`; barras de monitores
  secundários pela classe `Shell_SecondaryTrayWnd`.
* O varrimento corre a ~7 Hz, fora do caminho crítico do render.

### 5.3 Física (`sim.rs`)

Cada floco tem velocidade de queda própria, oscilação lateral harmónica
(fase/frequência/amplitude individuais) e sensibilidade ao vento. O vento é a
soma de três senos de períodos distintos (barato e sem ruído de alta frequência).

A acumulação é um **campo de alturas por superfície**: cada janela tem um vetor
de alturas (uma célula de 4 px por coluna do topo da janela). Quando um floco
atravessa o topo, o seu volume é distribuído por uma gaussiana estreita (com um
pequeno desvio lateral), o que forma grumos irregulares em vez de torres
alinhadas à grelha. As alturas sofrem difusão (alisamento) e deslizamento com
ângulo de repouso, além de derretimento lento; o máximo é 28 px (32 px na barra
de tarefas). Uma camada de base contínua cobre toda a superfície com neve, para
que as faixas deixadas pelo vento não apareçam como "buracos".

Reações:

* **mover/redimensionar** → o monte antigo desprende-se, escorrega com gravidade
  e desvanece; nasce um monte vazio na posição nova;
* **minimizar/fechar** → o monte desprende-se e desaparece;
* **Melt Snow** → derretimento acelerado (11 px/s) até desaparecer;
* **Off** → overlay escondido e estado limpo.

Occlusão: um topo tapado por outra janela mais acima na ordem Z **não** recebe
neve (nem a desenha), pelo que os flocos continuam a cair até uma superfície
realmente visível.

### 5.4 Renderização (`renderer.rs`)

1. `D3D11CreateDevice` (hardware; WARP em fallback) com `BGRA_SUPPORT`.
2. Swap-chain `CreateSwapChainForComposition` com
   `DXGI_ALPHA_MODE_PREMULTIPLIED` e flip model.
3. `IDCompositionDevice::CreateTargetForHwnd` + `IDCompositionVisual` — a
   transparência é composta pelo DWM, com alfa por píxel e aceleração de GPU.
4. Direct2D desenha sobre o back-buffer:
   * montes: um único *path* por troço visível, com o topo suavizado por
     Béziers (Catmull-Rom) e relevo multi-escala; gradiente branco→azulado e
     uma sombra ténue dão volume à camada;
   * flocos: `DrawBitmap` de uma textura 32×32 gerada em código (ponto branco
     com queda radial), escalada pela GPU.
5. `Present(sync=1)` sincroniza com o vblank.

O ritmo é fixado a 60 Hz por um **waitable timer de alta resolução**
(`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`): esperar em milissegundos com
`MsgWaitForMultipleObjects` tem granularidade de ~15,6 ms e limitava a animação a
~33 FPS.

### 5.5 Bandeja e arranque automático

`Shell_NotifyIconW` com uma janela de mensagens oculta; o ícone (floco de 6
braços) é rasterizado em código num DIB 32bpp e convertido com
`CreateIconIndirect`. O menu é nativo (`TrackPopupMenuEx` com `TPM_RETURNCMD`) e
é reconstruído em cada abertura para refletir o estado dos *checks*. O ícone é
recriado quando o Explorador reinicia (mensagem `TaskbarCreated`).

## 6. Desempenho medido

Ambiente de teste: Windows 11, 1920×1080, NVIDIA RTX 4050 Laptop, Rust 1.99.1,
build `--release`.

| Intensidade | Partículas | FPS | CPU (1 núcleo) | RAM (working set) |
|---|---|---|---|---|
| Light | ~120 | 58 | ~10 % | ~45 MB |
| Heavy | ~700 | 58 | ~16 % | ~43 MB |
| Off | 0 | — | ~0 % | ~15 MB (overlay escondido) |

O custo de CPU é dominado pelo número de flocos desenhados. A primeira versão
usava `FillEllipse` com antialiasing (~6 µs/floco em CPU, por tesselação);
passar a uma textura + `DrawBitmap` reduziu o custo para menos de metade.

## 7. Limitações conhecidas

* É um modelo 2.5D: a neve pousa na **primeira aresta horizontal visível** que
  encontra. Se o topo de uma janela estiver por baixo do topo visível de outra,
  recebe pouca ou nenhuma neve (não há simulação em profundidade).
* Janelas *fullscreen* exclusivas (jogos) ficam por cima do overlay; em modo
  janela/borderless funciona normalmente.
* O modo **Behind Windows** insere o overlay imediatamente abaixo do `WorkerW`
  do ambiente de trabalho (acima do papel de parede e abaixo dos ícones). É
  reafirmado a cada 2 s porque outras aplicações podem reorganizar a ordem Z.
* A neve não é desenhada em ecrãs bloqueados (`Winlogon`) nem durante a
  transição de desktop seguro.
* Sem persistência de preferências: o estado da sessão começa em `Steady` com
  acumulação ligada (pode ser alterado por linha de comandos).

## 8. Diagnóstico

Defina a variável de ambiente `SNOWHUSH_LOG=1` para escrever diagnóstico em
`%TEMP%\snowhush.log` (GPU escolhida, superfícies detetadas, FPS médio,
eventuais erros de render). Os erros fatais aparecem também numa caixa de
mensagem.

## 9. Testes

```powershell
cargo test
```

Oito testes cobrem a física (queda e deposição, oclusão de topos, degelo,
deslocamento/redimensionamento de janelas, fecho, desativação da acumulação e
topos no limite do ecrã) e a geração do ícone da bandeja.

## 10. Desinstalação

1. `Quit` no menu.
2. Se ativou `Open at Login`, desative-o (ou apague o valor `SnowHush` em
   `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`).
3. Apague a pasta do projeto/executável.
