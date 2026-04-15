use arc_swap::ArcSwap;
use std::sync::Arc;
use std::time::Instant;

/// Frame de video con datos BGRA (o UYVY) capturados del NDI.
/// Clone es necesario para que GameState (que hace snapshot) compile;
/// el clon copia el Vec pero solo ocurre en /test/grab, no en el game loop.
#[derive(Debug, Clone)]
pub struct Frame {
    pub width:       u32,
    pub height:      u32,
    /// Datos crudos. BGRA: 4 bytes por pixel → width*height*4 bytes.
    pub data:        Vec<u8>,
    /// Timestamp de cuando el NDI receiver publicó el frame.
    pub captured_at: Instant,
}

impl Frame {
    #[allow(dead_code)] // extension point: diagnostics
    pub fn byte_len(&self) -> usize {
        self.data.len()
    }
}

/// Wrapper sobre ArcSwap<Frame> para publicación/consumo lock-free.
///
/// Decisión de diseño: ArcSwap permite que el game loop lea el último
/// frame sin bloquear al thread NDI que lo publica, y viceversa.
/// El trade-off es que el game loop puede leer un frame "ya visto" si
/// el NDI no ha publicado uno nuevo — aceptable para 30 Hz.
pub struct FrameBuffer {
    inner: ArcSwap<Option<Frame>>,
}

impl FrameBuffer {
    pub fn new() -> Self {
        Self {
            inner: ArcSwap::new(Arc::new(None)),
        }
    }

    /// Publica un nuevo frame. Llamado solo por el thread NDI receiver.
    pub fn publish(&self, frame: Frame) {
        self.inner.store(Arc::new(Some(frame)));
    }

    /// Versión conveniente que devuelve directamente el Frame si existe.
    /// El clone copia los datos del frame — solo usar en código no crítico
    /// de latencia (HTTP /test/grab). El game loop puede usar load_full.
    pub fn latest_frame(&self) -> Option<Frame> {
        // load() retorna un Guard que se deref a Arc<Option<Frame>>.
        // Clonamos el Option<Frame> interno.
        (**self.inner.load()).clone()
    }

    /// Acceso lock-free al Arc del frame más reciente, sin copiar los datos.
    /// Usar en el game loop para máxima eficiencia.
    pub fn load_arc(&self) -> Arc<Option<Frame>> {
        self.inner.load_full()
    }
}

impl Default for FrameBuffer {
    fn default() -> Self { Self::new() }
}
