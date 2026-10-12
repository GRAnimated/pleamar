-- The password's way through: from the field to polkit's helper, and nowhere
-- else. It is not kept, not logged, and the field is emptied as it goes.

on("send", function()
    if not fact["auth.asking"] then return end
    sys.call("polkit.answer", text.answer)
    text.answer = ""
end)

on("dismiss", function()
    sys.call("polkit.cancel")
    text.answer = ""
end)
