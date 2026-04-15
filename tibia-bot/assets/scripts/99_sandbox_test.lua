-- 99_sandbox_test.lua — Script de verificación del sandbox.
--
-- Este código se ejecuta UNA VEZ al cargar el script (top-level). Si el
-- sandbox está funcionando, los `if` son todos falsy y se imprime
-- "[sandbox] check passed". Si ves "SANDBOX BROKEN", algo está mal.
--
-- No define ninguna función hook, así que no interfiere con example_healer.lua.

if io ~= nil then
    bot.log("error", "SANDBOX BROKEN: io disponible")
end
if os ~= nil then
    bot.log("error", "SANDBOX BROKEN: os disponible")
end
if package ~= nil then
    bot.log("error", "SANDBOX BROKEN: package disponible")
end
if require ~= nil then
    bot.log("error", "SANDBOX BROKEN: require disponible")
end
if dofile ~= nil then
    bot.log("error", "SANDBOX BROKEN: dofile disponible")
end
if loadfile ~= nil then
    bot.log("error", "SANDBOX BROKEN: loadfile disponible")
end
if debug ~= nil then
    bot.log("error", "SANDBOX BROKEN: debug disponible")
end

bot.log("info", "[sandbox] check passed")
