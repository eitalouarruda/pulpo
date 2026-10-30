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
