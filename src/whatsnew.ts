import type { Language } from "./i18n";

/// Novedades que se muestran después de actualizar, la más nueva primero.
///
/// Con cada cambio que vaya a una versión, añadir (o ampliar, si aún no se
/// publicó) una entrada arriba. El `id` no cambia nunca una vez publicado: es
/// lo que recuerda qué vio el usuario. La versión no se escribe aquí (la pone
/// el CI al publicar); Rust apunta la que la muestra por primera vez (ver
/// `whats_new.rs`).
export interface WhatsNewEntry {
  id: string;
  items: Record<Language, string[]>;
}

export const WHATS_NEW: WhatsNewEntry[] = [
  {
    id: "2026-11-12",
    items: {
      en: [
        "Smart Standby mode: apps you choose release RAM and background processes while not in use. Every app stays in the background by default; turn off \"Keep in background\" per app.",
        "Smooth auto-hiding sidebar on home screen and within web apps without screen flicker.",
        "YouTube Music mini player: fixed total track duration display and adaptive dock positioning.",
        "DevPulse bridge with sound alerts and unread counter badges.",
        "Downloads from every app in one place (Ctrl+J or Manage apps): progress of the ones in progress, then open, show in folder or clear. Files go straight to your Downloads folder.",
        "The sidebar auto-hide can be turned off in Manage apps.",
        "Thanks to @Nielmix for the first contributions to Pulpo! 🐙",
      ],
      es: [
        "Modo Standby inteligente: las apps que elijas liberan RAM y procesos en segundo plano cuando no las usas. Todas siguen en segundo plano por defecto; desactiva \"Mantener en segundo plano\" en cada app.",
        "Barra lateral con ocultamiento automático suave en la pantalla inicial y dentro de las apps sin parpadeos.",
        "Mini reproductor de YouTube Music: corregida la visualización de la duración total y dock adaptativo.",
        "Integración con DevPulse con alertas de sonido y contador de notificaciones.",
        "Descargas de todas las apps en un solo lugar (Ctrl+J o Administrar apps): el progreso de las que están en curso y luego abrir, mostrar en la carpeta o limpiar. Los archivos van directo a tu carpeta de Descargas.",
        "El ocultamiento automático de la barra lateral se puede desactivar en Administrar apps.",
        "¡Gracias a @Nielmix por las primeras contribuciones a Pulpo! 🐙",
      ],
      pt: [
        "Modo Standby inteligente: os apps que você escolher liberam memória RAM e processos em segundo plano quando não estão em uso. Todos continuam em segundo plano por padrão; desligue \"Manter em segundo plano\" em cada app.",
        "Barra lateral com recolhimento e revelação suave na tela inicial e dentro dos apps sem nenhum piscar de tela.",
        "Mini player do YouTube Music: corrigida a exibição da duração total da faixa musical e dock adaptativo.",
        "Integração com DevPulse com alertas sonoros e contador de notificações.",
        "Downloads de todos os apps em um só lugar (Ctrl+J ou Gerenciar apps): progresso dos que estão em andamento e depois abrir, mostrar na pasta ou limpar. Os arquivos vão direto para a sua pasta de Downloads.",
        "O recolhimento automático da barra lateral pode ser desligado em Gerenciar apps.",
        "Obrigado ao @Nielmix pelas primeiras contribuições ao Pulpo! 🐙",
      ],
    },
  },
  {
    id: "2026-11-03-macos-release",
    items: {
      en: ["macOS installers for Apple Silicon and Intel are now included in releases alongside Linux and Windows."],
      es: ["Los instaladores de macOS para Apple Silicon e Intel se incluyen en las releases junto con Linux y Windows."],
      pt: ["Os instaladores de macOS para Apple Silicon e Intel agora fazem parte das releases, junto com Linux e Windows."],
    },
  },
  {
    id: "2026-11-03-macos-files",
    items: {
      en: [
        "External file drops on Linux are forwarded to the visible web app, including attachment drop zones. Windows explicitly enables native file drops.",
        "Experimental macOS support with Chromium (CEF), separate sessions and Brave ad blocking; Intel and Apple Silicon builds are tested in GitHub Actions.",
      ],
      es: [
        "Los archivos arrastrados desde el explorador en Linux se entregan a la app visible, incluidas las zonas para adjuntar. Windows habilita explícitamente el arrastre nativo de archivos.",
        "Soporte experimental de macOS con Chromium (CEF), sesiones separadas y bloqueo de Brave; builds Intel y Apple Silicon en GitHub Actions.",
      ],
      pt: [
        "Arquivos arrastados do explorador no Linux são encaminhados ao app visível, incluindo áreas de anexos. No Windows, o recebimento nativo de arquivos fica explicitamente habilitado.",
        "Suporte experimental ao macOS com Chromium (CEF), sessões separadas e bloqueio Brave; builds Intel e Apple Silicon no GitHub Actions.",
      ],
    },
  },
  {
    id: "2026-11-03",
    items: {
      en: ["The home mini player now also works with Spotify (open.spotify.com), besides YouTube Music. Spotify is also in the app catalog."],
      es: ["El mini reproductor del inicio ahora también funciona con Spotify (open.spotify.com), además de YouTube Music. Spotify también está en el catálogo de apps."],
      pt: ["O mini player da tela inicial agora também funciona com o Spotify (open.spotify.com), além do YouTube Music. O Spotify também está no catálogo de apps."],
    },
  },
  {
    id: "2026-10-29",
    items: {
      en: [
        "Volume per app: a slider and mute button in the right-click menu, in Manage apps and when editing an app. Muted apps show 🔇 on their icon.",
        "Unread messages from all apps show up on Pulpo's taskbar icon (Windows) or dock (Linux docks that support counters); on Linux it is also written to ~/.cache/com.pulpo.app/badge.json for other bars.",
        "Arrange freely by dragging: apps and whole groups in the sidebar; the home page has its own order, with apps and sections you can move (dropping an app on another section moves it to that group).",
        "Closing Pulpo now closes everything: no app keeps running or playing music in the background.",
        "Windows: opening Pulpo while it's in the tray shows it instead of starting another copy (no more repeated tray icons).",
        "YouTube Music no longer shows a white background below the page.",
        "Wallhaven search finds many more wallpapers.",
        "This “What's new” window, after every update.",
      ],
      es: [
        "Volumen por app: barra y botón de silencio en el menú del botón derecho, en Gestionar apps y al editar una app. Las apps sin sonido llevan 🔇 en su icono.",
        "Los mensajes sin leer de todas las apps aparecen en el icono de Pulpo de la barra de tareas (Windows) o del dock (en Linux, los docks con contadores); en Linux también queda en ~/.cache/com.pulpo.app/badge.json para otras barras.",
        "Ordena a tu gusto arrastrando: apps y grupos enteros en la barra lateral; la página inicial tiene su propio orden, con apps y secciones que puedes mover (soltar una app en otra sección la pasa a ese grupo).",
        "Cerrar Pulpo ahora cierra todo: ninguna app sigue funcionando ni sonando en segundo plano.",
        "Windows: abrir Pulpo mientras está en la bandeja lo muestra en vez de abrir otro (se acabaron los iconos repetidos en la bandeja).",
        "YouTube Music ya no muestra un fondo blanco debajo de la página.",
        "La búsqueda de Wallhaven encuentra muchos más fondos.",
        "Esta ventana de “Novedades”, después de cada actualización.",
      ],
      pt: [
        "Volume por app: barra e botão de silenciar no menu do botão direito, em Gerenciar apps e ao editar um app. Apps sem som mostram 🔇 no ícone.",
        "As mensagens não lidas de todos os apps aparecem no ícone do Pulpo na barra de tarefas (Windows) ou no dock (no Linux, docks com contador); no Linux também fica em ~/.cache/com.pulpo.app/badge.json para outras barras.",
        "Organize do seu jeito arrastando: apps e grupos inteiros na barra lateral; a tela inicial tem sua própria ordem, com apps e seções que você pode mover (soltar um app em outra seção muda o grupo dele).",
        "Fechar o Pulpo agora fecha tudo: nenhum app continua rodando ou tocando música em segundo plano.",
        "Windows: abrir o Pulpo enquanto ele está na bandeja mostra o que já está aberto em vez de abrir outro (acabaram os ícones repetidos na bandeja).",
        "O YouTube Music não mostra mais um fundo branco embaixo da página.",
        "A busca do Wallhaven encontra muito mais papéis de parede.",
        "Esta janela de “O que há de novo”, depois de cada atualização.",
      ],
    },
  },
];
