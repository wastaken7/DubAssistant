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
- **Upload & Injeção Pós-Upload**:
  - Autenticação via `cookies.txt` (Netscape) ou header `Cookie` + extração automática de `_token` CSRF e `announce` URL.
  - Download do `.torrent` corrigido via **API Key** do site (`Authorization: Bearer`) com validação Bencode/InfoHash.
  - Injeção automática em **qBittorrent** (WebUI direto ou proxy `qui`), **Transmission**, **Deluge** e **Watch Folder** com mapeamento de caminhos e delay configurável.

## Ferramentas Externas Recomendadas

O `DubAssistant` detecta automaticamente as ferramentas no `PATH` (ou você pode apontar os caminhos na aba **Configurações**):

- **MediaInfo CLI** (`mediainfo`) — obrigatório para gerar `mediainfo.txt` e os campos de MediaInfo.
- **MKVToolNix** (`mkvmerge`, `mkvextract`) — recomendado para `.mkv`.
- **FFmpeg** (`ffmpeg`, `ffprobe`) — usado para `.mp4`, leitura de tags `TMDB` e discos DVD (`dvdvideo`).
- **eac3to** *(opcional, Windows)* — usado para listar e extrair playlists de discos Blu-ray (`BDMV`).

## Como Compilar Localmente

```bash
cargo test
cargo build --release
```

O executável será gerado em `target/release/DubAssistant.exe` (Windows) ou `target/release/DubAssistant` (Linux/macOS).

> **Linux**: instale as dependências de desenvolvimento do `egui`/`rfd` antes de compilar:
> ```bash
> sudo apt-get install -y libgtk-3-dev libxkbcommon-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libssl-dev pkg-config
> ```

## Licença

Distribuído sob a licença [MIT](LICENSE).
