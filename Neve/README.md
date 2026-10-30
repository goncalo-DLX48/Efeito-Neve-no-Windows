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
--carve-radius <20..80>                         raio (px) da escavação do cursor (omissão: 42)
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

Depois de mostrar o overlay, a aplicação chama
`ITaskbarList2::MarkFullscreenWindow(overlay, FALSE)`. Sem isso, o Windows trata
uma janela topmost do tamanho do monitor como uma aplicação *fullscreen* e
retira o `WS_EX_TOPMOST` à barra de tarefas, permitindo que janelas normais a
cubram. O topmost da barra é ainda verificado a cada 0,25 s; se tiver sido
retirado, o overlay é novamente marcado como não-fullscreen e a barra é
reinserida no topmost **abaixo** do overlay (para a neve continuar visível).

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
de alturas (uma célula de 2 px por coluna do topo da janela). Quando um floco
atravessa o topo, o seu volume é distribuído por uma gaussiana estreita (com um
pequeno desvio lateral). O relevo é alisado a cada tick por duas passagens do
filtro térmico `(h[i-1] + 2*h[i] + h[i+1]) / 4` e por um solver de ângulo de
repouso (8 passagens), produzindo curvas contínuas de fluido viscoso.

A altura é **estritamente limitada**:

* topo de janelas: **24 px**;
* barra de tarefas/fundo do ecrã: **48 px** (nunca tapa os ícones da barra).

Ao atingir o teto, a neve nova escorrega para as células vizinhas mais baixas
(até 80 px de distância); o que não couber derrete/desaparece. Uma camada de
base contínua cobre toda a superfície com neve, para que as faixas deixadas pelo
vento não apareçam como "buracos".

A renderização dos montes é **100% opaca** (o gradiente branco→azulado não usa
canal alfa).

#### Interação com o cursor

A posição global do rato é lida com `GetCursorPos` a cada frame (o overlay
continua `WS_EX_TRANSPARENT`, por isso os cliques atravessam normalmente).

* Quando o cursor está sobre a massa de neve de um monte, as células num raio
  configurável (`--carve-radius`, 20–80 px; omissão 42) são escavadas com
  falloff smoothstep (derivada nula nas extremidades), formando uma cratera
  côncava sem arestas.
* O volume removido é **conservado**: é empurrado para as cristas laterais, com
  a maior parte a seguir a direção do movimento (fica um monte à frente). O
  restante distribui-se atrás.
* Nos bordos interiores de uma janela o excesso **transborda como partículas
  ativas**, que caem sob gravidade a partir do canto e podem assentar numa
  janela inferior ou no fundo do ecrã. Nos limites laterais do ecrã (e da barra
  de tarefas) o excesso é **varrido para fora do monitor** e descartado.
* A neve no bordo de uma janela só transborda naturalmente quando está a menos
  de 3 px do teto (0,8× o excesso por segundo), evitando dentes permanentes.
* A marca de escavação (`hole`) atenua a camada de base e desvanece ao longo de
  ~1,2 s; o relaxamento térmico suaviza as paredes da cratera e recompõe-na
  gradualmente (mais depressa com neve nova).
* Se o cursor se mover depressa (> 220 px/s), são projetadas partículas soltas
  com velocidade lateral/ascendente; estas caem e voltam a assentar nos montes.
* A interação corre no thread principal, com custo O(células no raio) por frame,
  mantendo os 60 FPS.

Reações:

* **mover/redimensionar** → o monte antigo desprende-se, escorrega com gravidade
  e desvanece; nasce um monte vazio na posição nova;
* **minimizar/fechar** → o monte desprende-se e desaparece;
* **Melt Snow** → derretimento acelerado (120 px/s) até desaparecer;
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
   * montes: um único *path* por troço visível, amostrado a meia célula (2 px)
     e suavizado por Béziers (Catmull-Rom); relevo multi-escala e grão estático
     fino dão aspeto tátil; gradiente branco→azulado **opaco** e uma sombra
     ténue dão volume à camada;
   * flocos e partículas soltas: `DrawBitmap` de uma textura 32×32 gerada em
     código (ponto branco com queda radial), escalada pela GPU.
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
  recebe pouca ou nenhuma neve (não há simulação em profundidade). Pela mesma
  razão, a escavação do cursor remove a coluna de neve na abcissa do ponteiro
  (o perfil é unidimensional), em vez de abrir uma cratera apenas à superfície.
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

Dezassete testes cobrem a física (queda e deposição, oclusão de topos, degelo,
deslocamento/redimensionamento de janelas, fecho, desativação da acumulação e
topos no limite do ecrã) e a geração do ícone da bandeja.

## 10. Desinstalação

1. `Quit` no menu.
2. Se ativou `Open at Login`, desative-o (ou apague o valor `SnowHush` em
   `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`).
3. Apague a pasta do projeto/executável.
