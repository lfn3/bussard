# Bussard

Bussard is a library that allows you to embed a python WSGI application inside a [warp](https://github.com/seanmonstar/warp) filter.

At the moment it's _very_ crude, and barely works. You should probably not use it. Yet.

But assuming you are almost as crazy as I am - why would you want to do this? 

Perhaps you're not a fan of rewrites, especially the kind where you have to do 
a ton of work to catch up to a moving target.
Maybe you're looking to juice a bit more perf out of a couple of routes in your python app,
but don't have a the ambition to rewrite your whole application.
Maybe you desprately need access to some [fearless concurrency](https://doc.rust-lang.org/book/ch16-00-concurrency.html)
in your application.

There's still quite a bit of work to be done before I would consider using this anywhere near a production environment,
and the APIs are quite likely to change, so I'm not going to provide any guidance on how to use this yet.


### Why's it called Bussard?
Cause I'm a huge nerd, and Bussard Collectors 

![Enterprise E Bussard Collectors](https://vignette.wikia.nocookie.net/memoryalpha/images/3/3b/Bussard_collector%2C_2367.jpg/revision/latest?cb=20111106181904&path-prefix=en)

are used to provide fuel for the warp cores onboard Federation starships.
Obviously, this is designed to complement [warp](https://github.com/seanmonstar/warp)
And it is somewhat analogous to using a python webapp as the reaction mass for one written in rust,
which is what this was built for.