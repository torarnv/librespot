use protobuf::{self, Message};
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fmt;
use std::slice::bytes::copy_memory;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread;

use librespot_protocol as protocol;
use mercury::{MercuryRequest, MercuryMethod};
use util::{SpotifyId, FileId};
use session::Session;

pub trait MetadataTrait : Send + Any + 'static {
    type Message: protobuf::MessageStatic;
    fn from_msg(msg: &Self::Message) -> Self;
    fn base_url() -> &'static str;
    fn base_uri() -> &'static str;
    fn request(r: MetadataRef<Self>) -> MetadataRequest;
}

#[derive(Debug)]
pub struct Track {
    pub name: String,
    pub album: SpotifyId,
    pub files: Vec<FileId>
}

impl MetadataTrait for Track {
    type Message = protocol::metadata::Track;
    fn from_msg(msg: &Self::Message) -> Self {
        Track {
            name: msg.get_name().to_string(),
            album: SpotifyId::from_raw(msg.get_album().get_gid()),
            files: msg.get_file().iter()
                .map(|file| {
                    let mut dst = [0u8; 20];
                    copy_memory(&file.get_file_id(), &mut dst);
                    FileId(dst)
                })
                .collect(),
        }
    }
    fn base_url() -> &'static str {
        "hm://metadata/3/track"
    }
    fn base_uri() -> &'static str {
        "spotify:track"
    }
    fn request(r: MetadataRef<Self>) -> MetadataRequest {
        MetadataRequest::Track(r)
    }
}

#[derive(Debug)]
pub struct Album {
    pub name: String,
    pub artists: Vec<SpotifyId>,
    pub covers: Vec<FileId>
}

impl MetadataTrait for Album {
    type Message = protocol::metadata::Album;
    fn from_msg(msg: &Self::Message) -> Self {
        Album {
            name: msg.get_name().to_string(),
            artists: msg.get_artist().iter()
                .map(|a| SpotifyId::from_raw(a.get_gid()))
                .collect(),
            covers: msg.get_cover_group().get_image().iter()
                .map(|image| {
                    let mut dst = [0u8; 20];
                    copy_memory(&image.get_file_id(), &mut dst);
                    FileId(dst)
                })
                .collect(),
        }
    }
    fn base_url() -> &'static str {
        "hm://metadata/3/album"
    }
    fn base_uri() -> &'static str {
        "spotify:album"
    }
    fn request(r: MetadataRef<Self>) -> MetadataRequest {
        MetadataRequest::Album(r)
    }
}

#[derive(Debug)]
pub struct Artist {
    pub name: String,
}

impl MetadataTrait for Artist {
    type Message = protocol::metadata::Artist;
    fn from_msg(msg: &Self::Message) -> Self {
        Artist {
            name: msg.get_name().to_string(),
        }
    }
    fn base_url() -> &'static str {
        "hm://metadata/3/artist"
    }
    fn base_uri() -> &'static str {
        "spotify:artist"
    }
    fn request(r: MetadataRef<Self>) -> MetadataRequest {
        MetadataRequest::Artist(r)
    }
}

#[derive(Debug)]
pub enum MetadataState<T> {
    Loading,
    Loaded(T),
    Error,
}

pub struct Metadata<T: MetadataTrait> {
    id: SpotifyId,
    state: Mutex<MetadataState<T>>,
    cond: Condvar
}

pub type MetadataRef<T> = Arc<Metadata<T>>;

pub type TrackRef = MetadataRef<Track>;
pub type AlbumRef = MetadataRef<Album>;
pub type ArtistRef = MetadataRef<Artist>;

impl <T: MetadataTrait> Metadata<T> {
    pub fn id(&self) -> SpotifyId {
        self.id
    }

    pub fn lock(&self) -> MutexGuard<MetadataState<T>> {
        self.state.lock().unwrap()
    }

    pub fn wait(&self) -> MutexGuard<MetadataState<T>> {
        let mut handle = self.lock();
        while handle.is_loading() {
            handle = self.cond.wait(handle).unwrap();
        }
        handle
    }

    pub fn set(&self, state: MetadataState<T>) {
        let mut handle = self.lock();
        *handle = state;
        self.cond.notify_all();
    }

    pub fn get_uri(&self) -> String {
        format!("{}:{}", T::base_uri(), self.id.to_base62())
    }
}

impl <T: MetadataTrait + fmt::Debug> fmt::Debug for Metadata<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(f, "Metadata<>({:?}, {:?})", self.id, *self.lock())
    }
}

impl <T: MetadataTrait> MetadataState<T> {
    pub fn is_loading(&self) -> bool {
        match *self {
            MetadataState::Loading => true,
            _ => false
        }
    }

    pub fn is_loaded(&self) -> bool {
        match *self {
            MetadataState::Loaded(_) => true,
            _ => false
        }
    }

    pub fn unwrap<'s>(&'s self) -> &'s T {
        match *self {
            MetadataState::Loaded(ref data) => data,
            _ => panic!("Not loaded")
        }
    }
}

#[derive(Debug)]
pub enum MetadataRequest {
    Artist(ArtistRef),
    Album(AlbumRef),
    Track(TrackRef)
}

pub struct MetadataManager {
    cache: HashMap<(SpotifyId, TypeId), Box<Any + Send>>
}

impl MetadataManager {
    pub fn new() -> MetadataManager {
        MetadataManager {
            cache: HashMap::new()
        }
    }

    pub fn get<T: MetadataTrait>(&mut self, session: &Session, id: SpotifyId)
      -> MetadataRef<T> {
        let key = (id, TypeId::of::<T>());

        self.cache.get(&key)
            .and_then(|x| x.downcast_ref::<Weak<Metadata<T>>>())
            .and_then(|x| x.upgrade())
            .unwrap_or_else(|| {
                let x : MetadataRef<T> = Arc::new(Metadata{
                    id: id,
                    state: Mutex::new(MetadataState::Loading),
                    cond: Condvar::new()
                });

                self.cache.insert(key, Box::new(x.downgrade()));
                self.load(session, x.clone());
                x
            })
    }

    fn load<T: MetadataTrait> (&self, session: &Session, object: MetadataRef<T>) {
        let rx = session.mercury(MercuryRequest {
            method: MercuryMethod::GET,
            uri: format!("{}/{}", T::base_url(), object.id.to_base16()),
            content_type: None,
            payload: Vec::new()
        });

        thread::spawn(move || {
            let response = rx.into_inner();

            let msg : T::Message = protobuf::parse_from_bytes(
                response.payload.front().unwrap()).unwrap();

            object.set(MetadataState::Loaded(T::from_msg(&msg)));
        });
    }
}

