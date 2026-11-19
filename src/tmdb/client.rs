use crate::domain::models::Category;
use anyhow::{bail, Context, Result};
use regex::Regex;
use reqwest::Client;
use serde::Deserialize;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct TmdbSearchItem {
    pub id: u64,
    pub title_pt_br: String,
    pub title_original: String,
    pub year: String,
    pub category: Category,
}

#[derive(Debug, Clone)]
pub struct TmdbSeasonInfo {
    pub season_number: u32,
    pub year: String,
    pub episode_count: u32,
}

#[derive(Debug, Clone)]
pub struct PosterImage {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct TmdbDetails {
    pub id: u64,
    pub title_pt_br: String,
    pub title_original: String,
    pub year: String,
    pub category: Category,
    pub overview: String,
    pub genres: Vec<String>,
    pub vote_average: f32,
    pub runtime_label: String,
    pub poster: Option<PosterImage>,
    pub seasons: Vec<TmdbSeasonInfo>,
}

#[derive(Deserialize)]
struct GenreRaw {
    name: String,
}

#[derive(Deserialize)]
struct SearchMovieResp {
    results: Vec<MovieResult>,
}

#[derive(Deserialize)]
struct MovieResult {
    id: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    original_title: String,
    #[serde(default)]
    release_date: Option<String>,
    #[serde(default)]
    overview: String,
    #[serde(default)]
    poster_path: Option<String>,
    #[serde(default)]
    vote_average: f32,
    #[serde(default)]
    runtime: Option<u32>,
    #[serde(default)]
    genres: Vec<GenreRaw>,
}

#[derive(Deserialize)]
struct SearchTvResp {
    results: Vec<TvResult>,
}

#[derive(Deserialize)]
struct TvResult {
    id: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    original_name: String,
    #[serde(default)]
    first_air_date: Option<String>,
}

#[derive(Deserialize)]
struct TvDetailsResp {
    id: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    original_name: String,
    #[serde(default)]
    first_air_date: Option<String>,
    #[serde(default)]
    overview: String,
    #[serde(default)]
    poster_path: Option<String>,
    #[serde(default)]
    vote_average: f32,
    #[serde(default)]
    number_of_seasons: u32,
    #[serde(default)]
    number_of_episodes: u32,
    #[serde(default)]
    genres: Vec<GenreRaw>,
    #[serde(default)]
    seasons: Vec<TvSeasonRaw>,
}

#[derive(Deserialize)]
struct TvSeasonRaw {
    season_number: u32,
    #[serde(default)]
    air_date: Option<String>,
    #[serde(default)]
    episode_count: u32,
}

pub struct TmdbClient {
    http: Client,
    api_key: String,
}

impl TmdbClient {
    pub fn new(api_key: &str) -> Result<Self> {
        let key = api_key.trim().to_string();
        if key.is_empty() {
            bail!("Chave da API do TMDB não configurada. Adicione nas Configurações.");
        }
        let http = Client::builder().build()?;
        Ok(Self { http, api_key: key })
    }

    fn apply_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.api_key.len() > 40 || self.api_key.starts_with("eyJ") {
            req.bearer_auth(&self.api_key)
        } else {
            req.query(&[("api_key", self.api_key.as_str())])
        }
    }

    pub async fn search(&self, query: &str, category: Category) -> Result<Vec<TmdbSearchItem>> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }

        if q.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(id) = q.parse::<u64>() {
                let details = self.get_details(id, category).await?;
                return Ok(vec![TmdbSearchItem {
                    id: details.id,
                    title_pt_br: details.title_pt_br,
                    title_original: details.title_original,
                    year: details.year,
                    category,
                }]);
            }
        }

        match category {
            Category::Movie => {
                let req = self
                    .http
                    .get("https://api.themoviedb.org/3/search/movie")
                    .query(&[("language", "pt-BR"), ("query", q)]);
                let resp = self
                    .apply_auth(req)
                    .send()
                    .await
                    .context("Erro ao consultar TMDB search/movie")?;
                if !resp.status().is_success() {
                    bail!("TMDB retornou HTTP {}", resp.status());
                }
                let data: SearchMovieResp = resp.json().await?;
                Ok(data
                    .results
                    .into_iter()
                    .take(15)
                    .map(|m| TmdbSearchItem {
                        id: m.id,
                        title_pt_br: m.title,
                        title_original: m.original_title,
                        year: extract_year(m.release_date.as_deref()),
                        category: Category::Movie,
                    })
                    .collect())
            }
            Category::Series => {
                let req = self
                    .http
                    .get("https://api.themoviedb.org/3/search/tv")
                    .query(&[("language", "pt-BR"), ("query", q)]);
                let resp = self
                    .apply_auth(req)
                    .send()
                    .await
                    .context("Erro ao consultar TMDB search/tv")?;
                if !resp.status().is_success() {
                    bail!("TMDB retornou HTTP {}", resp.status());
                }
                let data: SearchTvResp = resp.json().await?;
                Ok(data
                    .results
                    .into_iter()
                    .take(15)
                    .map(|t| TmdbSearchItem {
                        id: t.id,
                        title_pt_br: t.name,
                        title_original: t.original_name,
                        year: extract_year(t.first_air_date.as_deref()),
                        category: Category::Series,
                    })
                    .collect())
            }
        }
    }

    pub async fn get_details(&self, id: u64, category: Category) -> Result<TmdbDetails> {
        match category {
            Category::Movie => {
                let url = format!("https://api.themoviedb.org/3/movie/{id}");
                let req = self.http.get(url).query(&[("language", "pt-BR")]);
                let resp = self.apply_auth(req).send().await?;
                if !resp.status().is_success() {
                    bail!("TMDB movie/{id} retornou HTTP {}", resp.status());
                }
                let m: MovieResult = resp.json().await?;
                let poster = match m.poster_path.as_deref() {
                    Some(p) if !p.is_empty() => self.fetch_poster(p).await.ok(),
                    _ => None,
                };
                let runtime_label = m
                    .runtime
                    .filter(|&r| r > 0)
                    .map(|r| format!("{}h {:02}min", r / 60, r % 60))
                    .unwrap_or_default();

                Ok(TmdbDetails {
                    id: m.id,
                    title_pt_br: m.title,
                    title_original: m.original_title,
                    year: extract_year(m.release_date.as_deref()),
                    category: Category::Movie,
                    overview: m.overview,
                    genres: m.genres.into_iter().map(|g| g.name).collect(),
                    vote_average: m.vote_average,
                    runtime_label,
                    poster,
                    seasons: Vec::new(),
                })
            }
            Category::Series => {
                let url = format!("https://api.themoviedb.org/3/tv/{id}");
                let req = self.http.get(url).query(&[("language", "pt-BR")]);
                let resp = self.apply_auth(req).send().await?;
                if !resp.status().is_success() {
                    bail!("TMDB tv/{id} retornou HTTP {}", resp.status());
                }
                let t: TvDetailsResp = resp.json().await?;
                let poster = match t.poster_path.as_deref() {
                    Some(p) if !p.is_empty() => self.fetch_poster(p).await.ok(),
                    _ => None,
                };
                let runtime_label = format!(
                    "{} temporada(s) · {} episódio(s)",
                    t.number_of_seasons, t.number_of_episodes
                );
                let seasons = t
                    .seasons
                    .into_iter()
                    .map(|s| TmdbSeasonInfo {
                        season_number: s.season_number,
                        year: extract_year(s.air_date.as_deref()),
                        episode_count: s.episode_count,
                    })
                    .collect();

                Ok(TmdbDetails {
                    id: t.id,
                    title_pt_br: t.name,
                    title_original: t.original_name,
                    year: extract_year(t.first_air_date.as_deref()),
                    category: Category::Series,
                    overview: t.overview,
                    genres: t.genres.into_iter().map(|g| g.name).collect(),
                    vote_average: t.vote_average,
                    runtime_label,
                    poster,
                    seasons,
                })
            }
        }
    }

    async fn fetch_poster(&self, poster_path: &str) -> Result<PosterImage> {
        let url = format!("https://image.tmdb.org/t/p/w342{poster_path}");
        let resp = self.http.get(&url).send().await?;
        if !resp.status().is_success() {
            bail!("Falha ao baixar pôster TMDB (HTTP {})", resp.status());
        }
        let bytes = resp.bytes().await?;
        let img = image::load_from_memory(&bytes)
            .context("Falha ao decodificar imagem do pôster TMDB")?
            .to_rgba8();
        let (w, h) = img.dimensions();
        Ok(PosterImage {
            width: w as usize,
            height: h as usize,
            rgba: img.into_raw(),
        })
    }
}

fn extract_year(date: Option<&str>) -> String {
    date.and_then(|d| d.get(0..4)).unwrap_or("").to_string()
}

/// Limpa nome de arquivo/pasta para sugerir um termo de busca no TMDB.
pub fn guess_search_query_from_name(raw_name: &str) -> String {
    static RE_CUT: OnceLock<Regex> = OnceLock::new();
    let re = RE_CUT.get_or_init(|| {
        Regex::new(r"(?i)\b(19\d{2}|20\d{2}|S\d{1,2}E\d{1,3}|S\d{1,2}|1080p|2160p|720p|480p|Blu-?Ray|WEB-?DL|WEBRip|REMUX|DVD|AC3|EAC3|DTS|TrueHD|x264|x265|H\.?264|H\.?265|HEVC)\b").unwrap()
    });
    let cleaned = raw_name.replace(['.', '_'], " ");
    let cut = if let Some(m) = re.find(&cleaned) {
        &cleaned[..m.start()]
    } else {
        &cleaned
    };
    cut.trim_matches(|c: char| c.is_whitespace() || c == '-' || c == '(' || c == '[')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
