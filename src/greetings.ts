/** Saludos de la página inicial, estilo Tabliss: uno por visita, según la hora. */

import type { Language } from "./i18n";

type Period = "morning" | "afternoon" | "evening" | "night";

const GREETINGS: Record<Language, Record<Period, string[]>> = {
  en: {
    morning: [
      "Good morning!",
      "Good morning. Coffee first, emails later.",
      "Rise and shine (the shining part is optional).",
      "Morning! Your inbox missed you. Probably.",
      "Good morning, early bird. The worms are in the other tab.",
      "A fresh day, zero unread regrets. So far.",
      "Good morning! Today's forecast: 80% chance of meetings.",
    ],
    afternoon: [
      "Good afternoon!",
      "Good afternoon. Lunch digested, productivity loading…",
      "Afternoon! Halfway there, keep going.",
      "Good afternoon. The post-lunch nap is calling. Don't answer.",
      "Hey there, afternoon warrior.",
      "Good afternoon! Time flies when you have 40 tabs open.",
      "Afternoon check-in: hydrated? Good.",
    ],
    evening: [
      "Good evening!",
      "Good evening. You've earned a break.",
      "Evening! The bugs can wait until tomorrow.",
      "Good evening. Time to close a few apps… or not.",
      "Hello, evening person. Dinner first?",
      "Good evening! Today is almost a wrap.",
      "Evening mode on: slippers recommended.",
    ],
    night: [
      "Good night!",
      "Still up? Bold choice.",
      "Good night. The internet will still be here tomorrow.",
      "Hello, night owl. 🦉",
      "Late night, huh? The keyboard won't tell anyone.",
      "It's late. Future you would like some sleep.",
      "Good night! Only bugs and owls are awake now.",
    ],
  },
  es: {
    morning: [
      "¡Buenos días!",
      "Buenos días. Primero el café, después los correos.",
      "¡A levantarse! (Lo de brillar es opcional).",
      "¡Buenos días! Tu bandeja de entrada te extrañó. Seguro.",
      "Buenos días, madrugador. Los gusanos están en la otra pestaña.",
      "Día nuevo, cero arrepentimientos sin leer. Por ahora.",
      "¡Buenos días! Pronóstico de hoy: 80 % de probabilidad de reuniones.",
    ],
    afternoon: [
      "¡Buenas tardes!",
      "Buenas tardes. Almuerzo digerido, productividad cargando…",
      "¡Buenas tardes! Ya vas por la mitad, sigue así.",
      "Buenas tardes. La siesta te llama. No contestes.",
      "¡Hola, guerrero de la tarde!",
      "¡Buenas tardes! El tiempo vuela con 40 pestañas abiertas.",
      "Control de la tarde: ¿tomaste agua? Bien.",
    ],
    evening: [
      "¡Buenas noches!",
      "Buenas noches. Te ganaste un descanso.",
      "¡Buenas noches! Los bugs pueden esperar a mañana.",
      "Buenas noches. Hora de cerrar algunas apps… o no.",
      "Hola, alma nocturna. ¿Primero la cena?",
      "¡Buenas noches! El día ya casi termina.",
      "Modo noche activado: se recomiendan pantuflas.",
    ],
    night: [
      "¿Todavía despierto? Qué valiente.",
      "Buenas noches. Internet seguirá aquí mañana.",
      "Hola, búho nocturno. 🦉",
      "Madrugada, ¿eh? El teclado no le dirá a nadie.",
      "Es tarde. Tu yo del futuro quiere dormir.",
      "¡Buenas noches! A esta hora solo están despiertos los bugs y los búhos.",
      "Las 3 de la mañana son para dormir, no para refactorizar.",
    ],
  },
  pt: {
    morning: [
      "Bom dia!",
      "Bom dia. Primeiro o café, depois os e-mails.",
      "Acorda pra vida! (Brilhar é opcional.)",
      "Bom dia! Sua caixa de entrada sentiu sua falta. Com certeza.",
      "Bom dia, madrugador. As minhocas estão na outra aba.",
      "Dia novo, zero arrependimentos não lidos. Por enquanto.",
      "Bom dia! Previsão de hoje: 80 % de chance de reuniões.",
    ],
    afternoon: [
      "Boa tarde!",
      "Boa tarde. Almoço digerido, produtividade carregando…",
      "Boa tarde! Já está na metade, continue assim.",
      "Boa tarde. A soneca está chamando. Não atenda.",
      "Olá, guerreiro da tarde!",
      "Boa tarde! O tempo voa com 40 abas abertas.",
      "Checagem da tarde: bebeu água? Ótimo.",
    ],
    evening: [
      "Boa noite!",
      "Boa noite. Você merece um descanso.",
      "Boa noite! Os bugs podem esperar até amanhã.",
      "Boa noite. Hora de fechar alguns apps… ou não.",
      "Olá, alma noturna. Primeiro o jantar?",
      "Boa noite! O dia já está quase acabando.",
      "Modo noite ativado: pantufas recomendadas.",
    ],
    night: [
      "Ainda acordado? Que coragem.",
      "Boa noite. A internet ainda vai estar aqui amanhã.",
      "Olá, coruja. 🦉",
      "Madrugada, é? O teclado não vai contar pra ninguém.",
      "Está tarde. O você do futuro quer dormir.",
      "Boa noite! A essa hora só os bugs e as corujas estão acordados.",
      "Três da manhã é pra dormir, não pra refatorar.",
    ],
  },
};

/** Mañana 5–12 h, tarde 12–19 h, noche 19–24 h, madrugada 0–5 h. */
export function periodOf(date: Date): Period {
  const h = date.getHours();
  if (h < 5) return "night";
  if (h < 12) return "morning";
  if (h < 19) return "afternoon";
  return "evening";
}

/** Un saludo al azar del periodo, distinto de `previous` si se puede. */
export function pickGreeting(lang: Language, period: Period, previous?: string): string {
  const list = GREETINGS[lang][period].filter((g) => g !== previous);
  return list[Math.floor(Math.random() * list.length)] ?? GREETINGS[lang][period][0];
}
