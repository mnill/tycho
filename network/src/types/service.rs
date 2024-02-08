use std::future::Future;
use std::marker::PhantomData;

use futures_util::future::BoxFuture;

pub trait Service<Request>: Send {
    type QueryResponse: Send + 'static;
    type OnQueryFuture: Future<Output = Option<Self::QueryResponse>> + Send + 'static;
    type OnMessageFuture: Future<Output = ()> + Send + 'static;
    type OnDatagramFuture: Future<Output = ()> + Send + 'static;

    /// Called when a query is received.
    ///
    /// Returns a future that resolves to the either response to the query if `Some`,
    /// or cancellation of the query if `None`.
    fn on_query(&mut self, req: Request) -> Self::OnQueryFuture;

    /// Called when a message is received.
    fn on_message(&mut self, req: Request) -> Self::OnMessageFuture;

    /// Called when a datagram is received.
    fn on_datagram(&mut self, req: Request) -> Self::OnDatagramFuture;
}

pub trait ServiceExt<Request>: Service<Request> {
    #[inline]
    fn boxed(self) -> BoxService<Request, Self::QueryResponse>
    where
        Self: Sized + Send + 'static,
        Self::OnQueryFuture: Send + 'static,
        Self::OnMessageFuture: Send + 'static,
        Self::OnDatagramFuture: Send + 'static,
    {
        BoxService::new(self)
    }

    #[inline]
    fn boxed_clone(self) -> BoxCloneService<Request, Self::QueryResponse>
    where
        Self: Clone + Sized + Send + 'static,
        Self::OnQueryFuture: Send + 'static,
        Self::OnMessageFuture: Send + 'static,
        Self::OnDatagramFuture: Send + 'static,
    {
        BoxCloneService::new(self)
    }
}

impl<T, Request> ServiceExt<Request> for T where T: Service<Request> + Send + ?Sized {}

impl<'a, S, Request> Service<Request> for &'a mut S
where
    S: Service<Request> + 'a,
{
    type QueryResponse = S::QueryResponse;
    type OnQueryFuture = S::OnQueryFuture;
    type OnMessageFuture = S::OnMessageFuture;
    type OnDatagramFuture = S::OnDatagramFuture;

    #[inline]
    fn on_query(&mut self, req: Request) -> Self::OnQueryFuture {
        <S as Service<Request>>::on_query(*self, req)
    }

    #[inline]
    fn on_message(&mut self, req: Request) -> Self::OnMessageFuture {
        <S as Service<Request>>::on_message(*self, req)
    }

    #[inline]
    fn on_datagram(&mut self, req: Request) -> Self::OnDatagramFuture {
        <S as Service<Request>>::on_datagram(*self, req)
    }
}

impl<S, Request> Service<Request> for Box<S>
where
    S: Service<Request> + ?Sized,
{
    type QueryResponse = S::QueryResponse;
    type OnQueryFuture = S::OnQueryFuture;
    type OnMessageFuture = S::OnMessageFuture;
    type OnDatagramFuture = S::OnDatagramFuture;

    #[inline]
    fn on_query(&mut self, req: Request) -> Self::OnQueryFuture {
        <S as Service<Request>>::on_query(self.as_mut(), req)
    }

    #[inline]
    fn on_message(&mut self, req: Request) -> Self::OnMessageFuture {
        <S as Service<Request>>::on_message(self.as_mut(), req)
    }

    #[inline]
    fn on_datagram(&mut self, req: Request) -> Self::OnDatagramFuture {
        <S as Service<Request>>::on_datagram(self.as_mut(), req)
    }
}

#[repr(transparent)]
pub struct BoxService<Request, Q> {
    inner: Box<
        dyn Service<
                Request,
                QueryResponse = Q,
                OnQueryFuture = BoxFuture<'static, Option<Q>>,
                OnMessageFuture = BoxFuture<'static, ()>,
                OnDatagramFuture = BoxFuture<'static, ()>,
            > + Send,
    >,
}

impl<Request, Q> BoxService<Request, Q> {
    pub fn new<S>(inner: S) -> Self
    where
        S: Service<Request, QueryResponse = Q> + Send + 'static,
        S::OnQueryFuture: Send + 'static,
        S::OnMessageFuture: Send + 'static,
        S::OnDatagramFuture: Send + 'static,
    {
        BoxService {
            inner: Box::new(BoxPinFutures(inner)),
        }
    }
}

impl<Request, Q> Service<Request> for BoxService<Request, Q>
where
    Request: Send + 'static,
    Q: Send + 'static,
{
    type QueryResponse = Q;
    type OnQueryFuture = BoxFuture<'static, Option<Q>>;
    type OnMessageFuture = BoxFuture<'static, ()>;
    type OnDatagramFuture = BoxFuture<'static, ()>;

    #[inline]
    fn on_query(&mut self, req: Request) -> Self::OnQueryFuture {
        self.inner.on_query(req)
    }

    #[inline]
    fn on_message(&mut self, req: Request) -> Self::OnMessageFuture {
        self.inner.on_message(req)
    }

    #[inline]
    fn on_datagram(&mut self, req: Request) -> Self::OnDatagramFuture {
        self.inner.on_datagram(req)
    }
}

#[repr(transparent)]
pub struct BoxCloneService<Request, Q> {
    inner: Box<
        dyn CloneService<
                Request,
                QueryResponse = Q,
                OnQueryFuture = BoxFuture<'static, Option<Q>>,
                OnMessageFuture = BoxFuture<'static, ()>,
                OnDatagramFuture = BoxFuture<'static, ()>,
            > + Send,
    >,
}

impl<Request, Q> BoxCloneService<Request, Q>
where
    Q: Send + 'static,
{
    pub fn new<S>(inner: S) -> Self
    where
        S: Service<Request, QueryResponse = Q> + Clone + Send + 'static,
        S::OnQueryFuture: Send + 'static,
        S::OnMessageFuture: Send + 'static,
        S::OnDatagramFuture: Send + 'static,
    {
        BoxCloneService {
            inner: Box::new(BoxPinFutures(inner)),
        }
    }
}

impl<Request, Q> Service<Request> for BoxCloneService<Request, Q>
where
    Request: Send + 'static,
    Q: Send + 'static,
{
    type QueryResponse = Q;
    type OnQueryFuture = BoxFuture<'static, Option<Q>>;
    type OnMessageFuture = BoxFuture<'static, ()>;
    type OnDatagramFuture = BoxFuture<'static, ()>;

    #[inline]
    fn on_query(&mut self, req: Request) -> Self::OnQueryFuture {
        self.inner.on_query(req)
    }

    #[inline]
    fn on_message(&mut self, req: Request) -> Self::OnMessageFuture {
        self.inner.on_message(req)
    }

    #[inline]
    fn on_datagram(&mut self, req: Request) -> Self::OnDatagramFuture {
        self.inner.on_datagram(req)
    }
}

impl<Request, Q> Clone for BoxCloneService<Request, Q>
where
    Q: Send + 'static,
{
    fn clone(&self) -> Self {
        BoxCloneService {
            inner: self.inner.clone_box(),
        }
    }
}

trait CloneService<Request>: Service<Request> {
    fn clone_box(
        &self,
    ) -> Box<
        dyn CloneService<
                Request,
                QueryResponse = Self::QueryResponse,
                OnQueryFuture = Self::OnQueryFuture,
                OnMessageFuture = Self::OnMessageFuture,
                OnDatagramFuture = Self::OnDatagramFuture,
            > + Send,
    >;
}

impl<Request, S> CloneService<Request> for S
where
    S: Service<Request> + Clone + Send + 'static,
    S::OnQueryFuture: Send + 'static,
    S::OnMessageFuture: Send + 'static,
    S::OnDatagramFuture: Send + 'static,
{
    fn clone_box(
        &self,
    ) -> Box<
        dyn CloneService<
                Request,
                QueryResponse = Self::QueryResponse,
                OnQueryFuture = Self::OnQueryFuture,
                OnMessageFuture = Self::OnMessageFuture,
                OnDatagramFuture = Self::OnDatagramFuture,
            > + Send,
    > {
        Box::new(self.clone())
    }
}

#[repr(transparent)]
struct BoxPinFutures<S>(S);

impl<S: Clone> Clone for BoxPinFutures<S> {
    #[inline]
    fn clone(&self) -> Self {
        BoxPinFutures(self.0.clone())
    }
}

impl<S, Request> Service<Request> for BoxPinFutures<S>
where
    S: Service<Request>,
{
    type QueryResponse = S::QueryResponse;
    type OnQueryFuture = BoxFuture<'static, Option<S::QueryResponse>>;
    type OnMessageFuture = BoxFuture<'static, ()>;
    type OnDatagramFuture = BoxFuture<'static, ()>;

    #[inline]
    fn on_query(&mut self, req: Request) -> Self::OnQueryFuture {
        Box::pin(self.0.on_query(req))
    }

    #[inline]
    fn on_message(&mut self, req: Request) -> Self::OnMessageFuture {
        Box::pin(self.0.on_message(req))
    }

    #[inline]
    fn on_datagram(&mut self, req: Request) -> Self::OnDatagramFuture {
        Box::pin(self.0.on_datagram(req))
    }
}

pub fn service_query_fn<T>(f: T) -> ServiceQueryFn<T> {
    ServiceQueryFn { f }
}

pub struct ServiceQueryFn<T> {
    f: T,
}

impl<T: Clone> Clone for ServiceQueryFn<T> {
    #[inline]
    fn clone(&self) -> Self {
        ServiceQueryFn { f: self.f.clone() }
    }
}

impl<Request, Q, T, F> Service<Request> for ServiceQueryFn<T>
where
    Q: Send + 'static,
    T: FnMut(Request) -> F + Send + 'static,
    F: Future<Output = Option<Q>> + Send + 'static,
{
    type QueryResponse = Q;
    type OnQueryFuture = F;
    type OnMessageFuture = futures_util::future::Ready<()>;
    type OnDatagramFuture = futures_util::future::Ready<()>;

    #[inline]
    fn on_query(&mut self, req: Request) -> Self::OnQueryFuture {
        (self.f)(req)
    }

    #[inline]
    fn on_message(&mut self, _req: Request) -> Self::OnMessageFuture {
        futures_util::future::ready(())
    }

    #[inline]
    fn on_datagram(&mut self, _req: Request) -> Self::OnDatagramFuture {
        futures_util::future::ready(())
    }
}

pub fn service_message_fn<Q, T>(f: T) -> ServiceMessageFn<Q, T> {
    ServiceMessageFn {
        f,
        _response: PhantomData,
    }
}

impl<Q, T: Clone> Clone for ServiceMessageFn<Q, T> {
    #[inline]
    fn clone(&self) -> Self {
        ServiceMessageFn {
            f: self.f.clone(),
            _response: PhantomData,
        }
    }
}

pub struct ServiceMessageFn<Q, T> {
    f: T,
    _response: PhantomData<Q>,
}

impl<Request, Q, T, F> Service<Request> for ServiceMessageFn<Q, T>
where
    Q: Send + 'static,
    T: FnMut(Request) -> F + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    type QueryResponse = Q;
    type OnQueryFuture = futures_util::future::Ready<Option<Q>>;
    type OnMessageFuture = F;
    type OnDatagramFuture = futures_util::future::Ready<()>;

    #[inline]
    fn on_query(&mut self, _req: Request) -> Self::OnQueryFuture {
        futures_util::future::ready(None)
    }

    #[inline]
    fn on_message(&mut self, req: Request) -> Self::OnMessageFuture {
        (self.f)(req)
    }

    #[inline]
    fn on_datagram(&mut self, _req: Request) -> Self::OnDatagramFuture {
        futures_util::future::ready(())
    }
}

pub fn service_datagram_fn<Q, T>(f: T) -> ServiceDatagramFn<Q, T> {
    ServiceDatagramFn {
        f,
        _response: PhantomData,
    }
}

pub struct ServiceDatagramFn<Q, T> {
    f: T,
    _response: PhantomData<Q>,
}

impl<Q, T: Clone> Clone for ServiceDatagramFn<Q, T> {
    #[inline]
    fn clone(&self) -> Self {
        ServiceDatagramFn {
            f: self.f.clone(),
            _response: PhantomData,
        }
    }
}

impl<Request, Q, T, F> Service<Request> for ServiceDatagramFn<Q, T>
where
    Q: Send + 'static,
    T: FnMut(Request) -> F + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    type QueryResponse = Q;
    type OnQueryFuture = futures_util::future::Ready<Option<Q>>;
    type OnMessageFuture = futures_util::future::Ready<()>;
    type OnDatagramFuture = F;

    #[inline]
    fn on_query(&mut self, _req: Request) -> Self::OnQueryFuture {
        futures_util::future::ready(None)
    }

    #[inline]
    fn on_message(&mut self, _req: Request) -> Self::OnMessageFuture {
        futures_util::future::ready(())
    }

    #[inline]
    fn on_datagram(&mut self, req: Request) -> Self::OnDatagramFuture {
        (self.f)(req)
    }
}
