// Runs once, on first start of an empty volume.
db.getSiblingDB("admin").createUser({
  user: "mqlc",
  pwd: "mqlc",
  roles: [{ role: "readAnyDatabase", db: "admin" }],
});
