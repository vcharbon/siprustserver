---
name: endurance
description: do not use unless explicitly told to
---


enhance te chaos tests sute to replicate all elements that make sense.
We have to ahve the sipp thest with loing call (as baseline), add the kill of primary sipp proxy and the abuse traffic (by default at 1 caps) when launchin endurence tests. 
Add the trafic peaks at 200 caps as chaos
To have better report, build a script to count sipp eror, concurent calls, and all the different failure cases  and have them reported as metrics by the grafana, with a dedicated dashboard. 

make sure the reporting sipp is wired the samme way as cluster start, then start a long endurence run with all the events at 5 caps long call and 100 caps short calls with chao every 15 minutes for 2 hours. 

Monitor chaos failure and metruics and delegate to subagent a thourough investigation, if fix is simplelnt (less than 100 loc do the fix and relaunc)