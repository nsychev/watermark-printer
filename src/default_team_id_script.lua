function get_team_id(ip_address)
    local parts = {}
    for num in ip_address:gmatch("%d+") do
        table.insert(parts, tonumber(num))
    end
    if #parts == 4 then
        return string.format("%03d", parts[3])
    end
    return nil
end