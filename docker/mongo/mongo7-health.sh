#!/bin/sh
# Healthcheck + replica set init for mongo7: initiates rs0 on first run, healthy once PRIMARY.
exec mongosh --quiet -u dbstudio -p dbstudio_test --authenticationDatabase admin --eval '
  const h = db.hello();
  if (h.setName === undefined) {
    rs.initiate({ _id: "rs0", members: [{ _id: 0, host: "mongo7:27017" }] });
    quit(1);
  }
  quit(h.isWritablePrimary ? 0 : 1);
'
