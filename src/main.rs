use async_std::{path::Path, task};
use scraper::{element_ref::ElementRef, Html, Selector};
use std::collections::HashSet;

type Result<T, E = Box<dyn std::error::Error>> = std::result::Result<T, E>;

#[derive(Debug)]
enum ScrapeError {
    RateLimited,
}

impl std::fmt::Display for ScrapeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for ScrapeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

fn unix_secs() -> u64 {
    use std::time;
    time::SystemTime::now()
        .duration_since(time::UNIX_EPOCH)
        .expect("Time went backwards")
        .as_secs()
}

use argh::FromArgs;

#[derive(FromArgs, Debug)]
#[argh(description = "iFunny Scraper")]
struct ScrapeOpt {
    /// username of the user to scrape
    #[argh(positional)]
    username: String,

    /// how many milliseconds to wait before scraping the next page of memes
    #[argh(option, short = 'd', default = "750")]
    page_scrape_delay_ms: u64,

    /// how many milliseconds to wait before scraping the next page of memes
    #[argh(option, short = 'p', default = "999")]
    num_pages: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    archive_user(argh::from_env()).await
}

async fn archive_user(opts: ScrapeOpt) -> Result<()> {
    use futures::stream::TryStreamExt;
    use std::time::Instant;
    let dl_folder = Path::new(&*opts.username);
    if !dl_folder.exists().await {
        std::fs::create_dir_all(&dl_folder)?;
    }
    let client = reqwest::Client::new();
    let before = Instant::now();
    let media_links = get_links(
        &client,
        &*opts.username,
        opts.num_pages,
        opts.page_scrape_delay_ms,
    )
    .await?;
    let link_count = media_links.len();
    let elapsed = before.elapsed();
    quick_save_links(media_links.as_slice(), dl_folder.join("links.txt")).await?;
    println!("Collected {} links in {:?}", link_count, elapsed);
    let before_dl = Instant::now();
    let futs = futures::stream::FuturesUnordered::new();
    for link in &media_links {
        futs.push(link.download(dl_folder.join(link.file_name())));
    }
    let _results: Vec<_> = futs.try_collect().await?;
    println!(
        "Downloaded {} items in {:?}",
        link_count,
        before_dl.elapsed()
    );
    Ok(())
}

async fn download_ifunny_image<P: AsRef<Path>>(url: &str, location: P) -> Result<()> {
    use image::GenericImageView;
    let image_reader = reqwest::get(url).await?.bytes().await?;
    let image = image::io::Reader::new(std::io::Cursor::new(image_reader))
        .with_guessed_format()?
        .decode()?;
    let crop = image.crop_imm(0, 0, image.width(), image.height() - 20);
    let mut fout = Vec::<u8>::new();
    crop.write_to(&mut fout, image::ImageFormat::Jpeg)?;
    async_std::fs::write(location, fout.as_slice()).await?;
    Ok(())
}

async fn download_ifunny_video<P: AsRef<Path>>(url: &str, location: P) -> Result<()> {
    let video_reader = reqwest::get(url).await?.bytes().await?;
    async_std::fs::write(location, video_reader).await?;
    Ok(())
}

// enum to Strip ifunny watermark off of photos
#[derive(Debug)]
enum IFunnyLink {
    Moving(String),
    Still(String),
}

async fn quick_save_links<P: AsRef<Path>>(links: &[IFunnyLink], location: P) -> Result<()> {
    let links_str = links
        .into_iter()
        .map(|l| l.url())
        .collect::<Vec<&str>>()
        .join("\n");
    async_std::fs::write(location, &links_str).await?;
    Ok(())
}

impl IFunnyLink {
    async fn download<P: AsRef<Path>>(&self, location: P) -> Result<()> {
        use IFunnyLink::*;
        match self {
            Moving(url) => download_ifunny_video(&url, location).await,
            Still(url) => download_ifunny_image(&url, location).await,
        }
    }

    fn file_name(&self) -> String {
        if let Some(file_name) = self.url().rsplitn(2, "/").next() {
            file_name.to_string()
        } else {
            format!("{}", unix_secs())
        }
    }

    fn url(&self) -> &str {
        use IFunnyLink::*;
        match self {
            Moving(url) => &url,
            Still(url) => &url,
        }
    }
}

async fn get_links(
    client: &reqwest::Client,
    username: &str,
    num_pages: usize,
    delay_ms: u64,
) -> Result<Vec<IFunnyLink>> {
    use std::io::Write;
    let mut links = Vec::new();
    let mut next_timestamp: Option<f64> = None;

    let posts_selector = Selector::parse("body").expect("Failed to construct posts CSS selector");
    let vid_media_selector = Selector::parse("div > div.post__media > div")
        .expect("Failed to construct video media CSS selector");
    let img_media_selector = Selector::parse("div > div.post__media > div > div > a > img")
        .expect("Failed to construct image media CSS selector");

    let mut seen_cache: HashSet<String> = HashSet::new();
    let mut duplicate_hits: usize = 0;

    for page in 0..num_pages {
        task::sleep(std::time::Duration::from_millis(delay_ms)).await;
        print!(
            "{}[2K\rPage {}/{} {} links",
            27 as char,
            page,
            num_pages,
            links.len()
        );
        std::io::stdout().flush()?;
        let time_unix = next_timestamp.unwrap_or_else(|| unix_secs() as f64);
        let url = format!(
            "https://ifunny.co/user/{}/timeline/{}?page={}&mode=list",
            username, time_unix, page
        );
        let html = client
            .get(&url)
            .header("X-Requested-With", "XMLHttpRequest")
            .header("Cookie", "mode=list;")
            .send()
            .await?
            .text()
            .await?;
        if html.is_empty() {
            println!();
            return Err(Box::new(ScrapeError::RateLimited));
        }
        // std::fs::write("latest.html", &html)?;
        let document = Html::parse_document(&html);
        if let Some(body) = document.select(&posts_selector).next() {
            for node in body.children() {
                if let Some(elem) = ElementRef::wrap(node) {
                    if let Some(ts) = elem
                        .value()
                        .attr("data-next")
                        .map(|ts| ts.parse::<f64>().ok())
                        .flatten()
                    {
                        next_timestamp = Some(ts);
                    }
                    match elem.select(&vid_media_selector).next() {
                        Some(e) => {
                            if let Some(source) = e.value().attr("data-source") {
                                let owned = source.to_string();
                                if seen_cache.contains(&owned) {
                                    duplicate_hits += 1;
                                    if duplicate_hits >= 2 {
                                        println!();
                                        return Ok(links);
                                    }
                                } else {
                                    seen_cache.insert(owned.clone());
                                    links.push(IFunnyLink::Moving(owned));
                                }
                            } else {
                                match elem.select(&img_media_selector).next() {
                                    Some(e) => {
                                        if let Some(source) = e.value().attr("data-src") {
                                            let owned = source.to_string();
                                            if seen_cache.contains(&owned) {
                                                duplicate_hits += 1;
                                                if duplicate_hits >= 2 {
                                                    println!();
                                                    return Ok(links);
                                                }
                                            } else {
                                                seen_cache.insert(owned.clone());
                                                links.push(IFunnyLink::Still(owned));
                                            }
                                        }
                                    }
                                    None => {}
                                }
                            }
                        }
                        None => {}
                    }
                }
            }
        }
    }

    println!();
    Ok(links)
}
