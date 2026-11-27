# DubAssistant

Aplicativo desktop nativo para facilitar a extração lossless, padronização de nomes, geração de `.torrent` privado, envio de dublagens e injeção automática no cliente BitTorrent.

## Funcionalidades

- **3 Modos de Entrada (Filmes e Séries / Temporada Completa)**:
  - **Vídeo(s) Pronto(s) (`.mkv` / `.mp4`)**: extrai áudio puro (sem recodificar) + legendas selecionadas em **uma única leitura** do arquivo (`mkvextract` / `ffmpeg -c copy`), capítulos e `mediainfo.txt`.
  - **Disco Raw (Blu-ray `BDMV` / DVD `VIDEO_TS` e `.ISO`)**: lista playlists/títulos, extrai Blu-ray em leitura única via `eac3to` (com `-keepDialnorm`) e DVD via `ffmpeg -f dvdvideo`.
  - **Pasta já Extraída**: valida arquivos contra as regras do acervo (bloqueando extensões e containers proibidos como `.mka`, `.m4a`, `.mp3`, `.opus`, `.zip`, `.rar`) e organiza na pasta final.
- **Integração com TMDB**:
  - Detecta automaticamente tags `TMDB` embutidas no MKV (ex: `movie/1368337`, `tv/66732`).
  - Exibe pôster, títulos PT-BR/original, ano, nota, gêneros, duração/temporadas e sinopse.
  - Preenche automaticamente o padrão oficial de nomenclatura e detecta a Fonte (`Blu-ray`, `WEB-DL`, `DVD`, `TV / VHS`) pelo título/codec.
- **Upload 100% via API Oficial & Injeção Pós-Upload**:
  - Envio direto via `POST https://capybarabr.com/api/dubs/upload` usando apenas **API Key** (`Authorization: Bearer`, sem necessidade de cookies nem CSRF), com bloqueio nativo de duplicatas pelo servidor (`HTTP 422`).
  - Download automático do `.torrent` corrigido via `download_link` da resposta da API com validação Bencode/InfoHash.
  - Injeção automática em **qBittorrent** (WebUI direto ou proxy `qui`), **Transmission**, **Deluge** e **Watch Folder** com mapeamento de caminhos e delay configurável.

- **Análise de Áudio na Descrição** *(ligada por padrão)*:
  - Após o processamento, cada áudio é decodificado uma única vez (`ffmpeg` + análise em streaming, RAM constante) e um relatório BBCode compacto é anexado à descrição (editável antes do envio).
  - Mostra formato, duração (com Δ em relação ao vídeo de referência e FPS), loudness EBU R128 (LUFS, LRA, true peak BS.1770), dialnorm de AC3/E-AC3, largura de banda e clipping, com níveis por canal em `[spoiler]`.
  - Gera `[alert]`/`[note]` apenas quando algo é detectado: surrounds/LFE/central sem sinal, upmix (surround copiando frontal, mono replicado em L/C/R), estéreo dual-mono, surround matricial (Pro Logic), silêncio digital no meio do áudio, diferença de duração, clipping e corte abrupto em codec lossless (indício de transcode).
  - Temporadas viram uma tabela com uma linha por episódio. Desative em **Análise de áudio na descrição** (GUI), `config-set --audio-report false` ou `upload --no-audio-report`.

## Ferramentas Externas Recomendadas

O `DubAssistant` detecta automaticamente as ferramentas no `PATH` (ou você pode apontar os caminhos na aba **Configurações**):

- **MediaInfo CLI** (`mediainfo`) — obrigatório para gerar `mediainfo.txt` e os campos de MediaInfo.
- **MKVToolNix** (`mkvmerge`, `mkvextract`) — recomendado para `.mkv`.
- **FFmpeg** (`ffmpeg`, `ffprobe`) — usado para `.mp4`, leitura de tags `TMDB` e discos DVD (`dvdvideo`).
- **eac3to** *(opcional, Windows)* — usado para listar e extrair playlists de discos Blu-ray (`BDMV`).

## Modo 100% CLI (`dubassistant-cli`)

Além da interface gráfica (`DubAssistant`), o projeto inclui o executável de terminal **`dubassistant-cli`** (compartilha o mesmo `config.json` da GUI e suporta cancelamento via `Ctrl+C`):

```bash
# Ver configuração e ferramentas detectadas
dubassistant-cli config-info

# Testar API Key do CapybaraBR
dubassistant-cli test-api

# Configurar API Key, Announce URL e TMDB pelo terminal
dubassistant-cli config-set --capybara-api-key "SUA_CHAVE" --announce "https://capybarabr.com/announce/SEU_PASSKEY" --tmdb-api-key "SUA_CHAVE_TMDB"

# Inspecionar faixas de um vídeo, disco raw (BDMV/VIDEO_TS/.ISO) ou pasta extraída
dubassistant-cli inspect "Filme.2024.1080p.WEB-DL.mkv"

# Verificar duplicatas entre as dublagens pendentes de moderação (as já aprovadas são bloqueadas pelo servidor no envio)
dubassistant-cli dupe-check --title "As Tartarugas Ninja" --codec AC3 --source Blu-ray

# Processar + dupe-check + espectrograma + upload + injeção no cliente (use --dry-run para testar sem enviar)
dubassistant-cli upload "Filme.2024.1080p.WEB-DL.mkv" --version streaming --studio "Delart" --spectrogram --dry-run

# Ver o relatório de análise de áudio de um arquivo (faixa 0 = primeira faixa de áudio)
dubassistant-cli analyze-audio "Filme.mkv" --track 0 --reference "Filme.mkv" --fps 23.976

# Blu-ray raw escolhendo playlist e faixa de áudio específica
dubassistant-cli upload "D:/Blu-ray/Filme" --playlist 1 --audio-track 4 --version cinema
```

## Como Compilar Localmente

```bash
cargo test
cargo build --release
```

Os executáveis serão gerados em `target/release/DubAssistant.exe` (GUI) e `target/release/dubassistant-cli.exe` (CLI) no Windows (ou sem `.exe` no Linux/macOS).

> **Linux**: instale as dependências de desenvolvimento do `egui`/`rfd` antes de compilar:
> ```bash
> sudo apt-get install -y libgtk-3-dev libxkbcommon-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libssl-dev pkg-config
> ```

## Licença

Distribuído sob a licença [MIT](LICENSE).
