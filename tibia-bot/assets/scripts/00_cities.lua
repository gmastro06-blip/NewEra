-- 00_cities.lua — Tabla global CITIES con coordenadas de referencia.
--
-- Se carga antes que cualquier otro script (orden alfabetico).
-- Uso desde otros scripts:
--
--   local depot = CITIES.abdendriel.depot
--   bot.log("info", "Depot Ab'Dendriel: " .. depot.x .. "," .. depot.y .. "," .. depot.z)
--
--   local npc = CITIES.thais.potions
--   bot.say("hi")  -- estando cerca de (npc.x, npc.y, npc.z)
--
-- Cada ciudad tiene:
--   .depot    = { x, y, z }          -- cofre del depot / banco
--   .temple   = { x, y, z }          -- templo (respawn)
--   .potions  = { x, y, z, npc="" }  -- NPC que vende pociones
--
-- Fuentes: tibiamaps/tibia-map-data markers.json, OpenTibiaBR Canary,
--          TibiaWiki NPC spawn data. Fecha: 2026-04-12.

CITIES = {

  thais = {
    name = "Thais",
    depot   = { x = 32347, y = 32226, z = 7 },
    temple  = { x = 32369, y = 32241, z = 7 },
    potions = { x = 32398, y = 32222, z = 7, npc = "Xodet" },
  },

  carlin = {
    name = "Carlin",
    depot   = { x = 32336, y = 31784, z = 7 },
    temple  = { x = 32360, y = 31782, z = 7 },
    potions = { x = 32343, y = 31828, z = 7, npc = "Rachel" },
  },

  abdendriel = {
    name = "Ab'Dendriel",
    depot   = { x = 32682, y = 31685, z = 7 },
    temple  = { x = 32732, y = 31634, z = 7 },
    potions = { x = 32669, y = 31657, z = 6, npc = "Shiriel" },
  },

  venore = {
    name = "Venore",
    depot   = { x = 32957, y = 32076, z = 7 },
    temple  = { x = 32957, y = 32076, z = 7 },
    potions = { x = 32971, y = 32088, z = 6, npc = "Digger" },
  },

  kazordoon = {
    name = "Kazordoon",
    depot   = { x = 32657, y = 31910, z = 8 },
    temple  = { x = 32649, y = 31925, z = 11 },
    potions = { x = 32630, y = 31919, z = 5, npc = "Sigurd" },
  },

  edron = {
    name = "Edron",
    depot   = { x = 33169, y = 31812, z = 8 },
    temple  = { x = 33217, y = 31814, z = 8 },
    potions = { x = 33256, y = 31839, z = 3, npc = "Alexander" },
  },

  darashia = {
    name = "Darashia",
    depot   = { x = 33214, y = 32455, z = 8 },
    temple  = { x = 33213, y = 32454, z = 1 },
    potions = { x = 33220, y = 32403, z = 7, npc = "Asima" },
  },

  ankrahmun = {
    name = "Ankrahmun",
    depot   = { x = 33127, y = 32843, z = 7 },
    temple  = { x = 33194, y = 32853, z = 8 },
    potions = { x = 33130, y = 32811, z = 5, npc = "Mehkesh" },
  },

  port_hope = {
    name = "Port Hope",
    depot   = { x = 32623, y = 32746, z = 7 },
    temple  = { x = 32594, y = 32745, z = 7 },
    potions = { x = 32621, y = 32740, z = 5, npc = "Tandros" },
  },

  liberty_bay = {
    name = "Liberty Bay",
    depot   = { x = 32327, y = 32835, z = 7 },
    temple  = { x = 32317, y = 32826, z = 7 },
    potions = { x = 32345, y = 32808, z = 7, npc = "Frederik" },
  },

  svargrond = {
    name = "Svargrond",
    depot   = { x = 32263, y = 31140, z = 7 },
    temple  = { x = 32212, y = 31132, z = 7 },
    potions = { x = 32307, y = 31134, z = 7, npc = "Nelly" },
  },

  yalahar = {
    name = "Yalahar",
    depot   = { x = 32793, y = 31248, z = 7 },
    temple  = { x = 32787, y = 31276, z = 7 },
    potions = { x = 32790, y = 31236, z = 5, npc = "Chuckles" },
  },

  farmine = {
    name = "Farmine",
    depot   = { x = 33019, y = 31458, z = 10 },
    temple  = { x = 33023, y = 31521, z = 11 },
    -- Rabaz requiere completar The New Frontier Quest mision 8.
    potions = { x = 32992, y = 31471, z = 3, npc = "Rabaz" },
  },

  roshamuul = {
    name = "Roshamuul",
    depot   = { x = 33553, y = 32389, z = 7 },
    temple  = { x = 33513, y = 32363, z = 6 },
    potions = { x = 33542, y = 32383, z = 7, npc = "Asnarus" },
  },

  rathleton = {
    name = "Rathleton",
    depot   = { x = 33657, y = 31657, z = 7 },
    temple  = { x = 33594, y = 31899, z = 6 },
    potions = { x = 33619, y = 31884, z = 4, npc = "Alaistar" },
  },

  issavi = {
    name = "Issavi",
    depot   = { x = 33920, y = 31480, z = 7 },
    temple  = { x = 33921, y = 31477, z = 5 },
    potions = { x = 33910, y = 31514, z = 7, npc = "Faloriel" },
  },

  marapur = {
    name = "Marapur",
    depot   = { x = 33776, y = 32840, z = 7 },
    temple  = { x = 33842, y = 32853, z = 7 },
    potions = { x = 33778, y = 32837, z = 6, npc = "Nipuna" },
  },

}

-- Helpers utilitarios

--- Retorna la ciudad mas cercana al punto (x, y, z) dado.
--- Usa distancia Manhattan en el plano XY (ignora Z).
--- Ejemplo: local city = nearest_city(32370, 32240, 7)  --> "thais"
function nearest_city(px, py, pz)
  local best_key = nil
  local best_dist = 999999
  for key, city in pairs(CITIES) do
    local d = math.abs(city.depot.x - px) + math.abs(city.depot.y - py)
    if d < best_dist then
      best_dist = d
      best_key = key
    end
  end
  return best_key
end

--- Retorna distancia Chebyshev entre dos puntos (para estimar tiles de caminata).
function tile_distance(x1, y1, x2, y2)
  return math.max(math.abs(x2 - x1), math.abs(y2 - y1))
end
