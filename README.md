

- insert 

```bash
curl -X POST -d 'username=peter&message=你妈叫你吃饭' 'localhost:8080'
```


-query 

```bash
curl localhost:8080

# or

# more query param !!!
curl localhost:8080?before=<timestamp>&after=<timestamp>

```

or

input "localhost:8080" into chrome browser.